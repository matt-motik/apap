//! Producer/Consumer audio engine (ТЗ A2.0 §5): decoding and resampling run on
//! a dedicated worker thread that fills a lock-free SPSC ring; the cpal
//! real-time callback is a pure consumer that only touches atomics and the ring.
//!
//! Layout:
//! - [`PlaybackWorker`] — owns `Box<dyn AudioSource>` + [`Resampler`], pushes
//!   interleaved f32 at the final device rate into an `rtrb::Producer`.
//! - [`RtShared`] — the only state shared between UI, worker and callback: plain
//!   atomics plus immutable stream geometry (`out_rate`/`out_ch`). No `Mutex`.
//! - [`RtConsumer`] — moved into the cpal callback: ring consumer + preallocated
//!   scratch. Never allocates, never locks.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::decoder::AudioSource;
use super::output::Resampler;

/// Hard ceiling for the preallocated f32 scratch pool (interleaved samples).
/// Large enough to cover any realistic `cpal` callback buffer (65536 samples ≈
/// 8K stereo frames @44.1 kHz). Pinned at stream open so neither the worker nor
/// the real-time consumer ever grows it (ТЗ A2.0 §3.1).
pub const MAX_OUT_SAMPLES: usize = 1 << 16;

/// Output frames produced by one worker iteration. Small enough to keep seek
/// latency low, large enough to amortise the ring round-trips.
const WORKER_CHUNK_FRAMES: usize = 1024;

/// Shared, lock-free control/status block. Read by the worker and the real-time
/// consumer, written by the UI thread (and by whichever side owns each counter).
pub struct RtShared {
    /// Software volume as raw `f32` bits (atomic f32 without a lock).
    volume_bits: AtomicU32,
    muted: AtomicBool,
    /// Transport: whether the worker should produce and the consumer should play.
    playing: AtomicBool,
    /// `true` when the track is logically finished (stop or natural EOF).
    finished: AtomicBool,
    /// Direct Output: bypass software volume/mute in the consumer.
    bit_perfect: AtomicBool,
    /// Dither-mode index (see `player::dither_index`).
    dither: AtomicU8,
    /// Immutable after stream open: whether the resampler actually transforms
    /// the stream (drives the «bit-perfect not guaranteed» badge).
    resampler_enabled: bool,
    /// Output frame counter owned by the consumer (advanced while playing).
    pos_frames: AtomicU64,
    /// Seek handshake: the UI bumps `seek_generation` and publishes the target;
    /// the worker resets the decoder/resampler and echoes `seek_done_generation`;
    /// the consumer drains the ring and re-bases its position once they match.
    seek_generation: AtomicU64,
    seek_target_frames: AtomicU64,
    seek_done_generation: AtomicU64,
    /// Set by the worker when the source is exhausted and the tail is drained.
    eof: AtomicBool,
    /// Set by the consumer when playback ends because of `eof`.
    natural_end: AtomicBool,
    /// Non-zero when the worker hit an unrecoverable error (diagnostics).
    rt_err: AtomicU8,
    /// Visualizer tap switch (read by the worker); the producer itself lives in
    /// the worker and is installed via [`WorkerCmd::SetVizTap`].
    viz_tap_active: AtomicBool,
    /// Set to true by [`PlaybackWorker::stop`]; lets the worker bail out even
    /// while blocked pushing into a full ring (the consumer has stopped).
    stop_requested: AtomicBool,
    /// Immutable stream geometry, published once at open.
    out_rate: u32,
    out_ch: usize,
}

impl RtShared {
    pub fn new(out_rate: u32, out_ch: usize, resampler_enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            volume_bits: AtomicU32::new(0.8f32.to_bits()),
            muted: AtomicBool::new(false),
            playing: AtomicBool::new(false),
            finished: AtomicBool::new(true),
            bit_perfect: AtomicBool::new(false),
            dither: AtomicU8::new(crate::audio::player::DITHER_INDEX_TPDF),
            resampler_enabled,
            pos_frames: AtomicU64::new(0),
            seek_generation: AtomicU64::new(0),
            seek_target_frames: AtomicU64::new(0),
            seek_done_generation: AtomicU64::new(0),
            eof: AtomicBool::new(false),
            natural_end: AtomicBool::new(false),
            rt_err: AtomicU8::new(0),
            viz_tap_active: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            out_rate,
            out_ch,
        })
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    pub fn out_ch(&self) -> usize {
        self.out_ch
    }

    pub fn resampler_enabled(&self) -> bool {
        self.resampler_enabled
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    pub fn set_volume(&self, v: f32) {
        self.volume_bits.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    pub fn toggle_muted(&self) {
        let cur = self.muted.load(Ordering::Relaxed);
        self.muted.store(!cur, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn set_playing(&self, p: bool) {
        self.playing.store(p, Ordering::Relaxed);
    }

    pub fn effective_volume(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            self.volume()
        }
    }

    pub fn bit_perfect(&self) -> bool {
        self.bit_perfect.load(Ordering::Relaxed)
    }

    pub fn set_bit_perfect(&self, b: bool) {
        self.bit_perfect.store(b, Ordering::Relaxed);
    }

    /// True when bit-perfect is on but the stream is resampled anyway.
    pub fn bit_perfect_resampled(&self) -> bool {
        self.bit_perfect() && self.resampler_enabled
    }

    pub fn dither_index(&self) -> u8 {
        self.dither.load(Ordering::Relaxed)
    }

    pub fn set_dither_index(&self, idx: u8) {
        self.dither.store(idx, Ordering::Relaxed);
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    pub fn set_finished(&self, f: bool) {
        self.finished.store(f, Ordering::Relaxed);
    }

    pub fn pos_frames(&self) -> u64 {
        self.pos_frames.load(Ordering::Relaxed)
    }

    pub fn set_pos_frames(&self, frames: u64) {
        self.pos_frames.store(frames, Ordering::Relaxed);
    }

    pub fn natural_end(&self) -> bool {
        self.natural_end.load(Ordering::Relaxed)
    }

    pub fn set_natural_end(&self, e: bool) {
        self.natural_end.store(e, Ordering::Relaxed);
    }

    pub fn eof(&self) -> bool {
        self.eof.load(Ordering::Relaxed)
    }

    pub fn set_eof(&self, e: bool) {
        self.eof.store(e, Ordering::Relaxed);
    }

    /// Publish a seek request for the worker and return its generation. The
    /// caller follows up with [`WorkerCmd::Seek`].
    pub fn begin_seek(&self, target_frames: u64) -> u64 {
        self.seek_target_frames.store(target_frames, Ordering::Relaxed);
        self.seek_generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn seek_generation(&self) -> u64 {
        self.seek_generation.load(Ordering::Relaxed)
    }

    pub fn seek_target_frames(&self) -> u64 {
        self.seek_target_frames.load(Ordering::Relaxed)
    }

    fn publish_seek_done(&self, generation: u64) {
        self.seek_done_generation
            .store(generation, Ordering::Release);
    }

    pub fn viz_tap_active(&self) -> bool {
        self.viz_tap_active.load(Ordering::Relaxed)
    }

    pub fn set_viz_tap_active(&self, active: bool) {
        self.viz_tap_active.store(active, Ordering::Relaxed);
    }

    pub fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Relaxed)
    }

    pub fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Relaxed);
    }
}

/// Consumer half of the ring plus the control block, owned exclusively by the
/// real-time cpal callback. Holds a preallocated scratch buffer so the
/// non-`f32` callbacks can pull f32 first and quantise in place (no alloc).
pub struct RtConsumer {
    ring: rtrb::Consumer<f32>,
    shared: Arc<RtShared>,
    scratch: Vec<f32>,
    /// Callback-local TPDF noise generator for the i16/u8/i32 quantisers.
    tpdf: crate::audio::player::TpdfRng,
    /// Last seek generation reconciled with the worker.
    generation: u64,
}

impl RtConsumer {
    pub fn new(ring: rtrb::Consumer<f32>, shared: Arc<RtShared>) -> Self {
        let generation = shared.seek_generation();
        Self {
            ring,
            shared,
            scratch: vec![0.0f32; MAX_OUT_SAMPLES],
            tpdf: crate::audio::player::TpdfRng::new(),
            generation,
        }
    }

    pub fn shared(&self) -> &Arc<RtShared> {
        &self.shared
    }

    /// Copy of the TPDF generator (cheap: one `u32` of state).
    pub fn tpdf(&self) -> crate::audio::player::TpdfRng {
        self.tpdf
    }

    pub fn set_tpdf(&mut self, tpdf: crate::audio::player::TpdfRng) {
        self.tpdf = tpdf;
    }

    /// Reconcile a pending seek. Returns `true` when the ring is safe to read;
    /// `false` means the worker has not finished the seek yet, so the callback
    /// must emit silence for this buffer. Drains stale frames exactly once per
    /// generation (ТЗ A2.0 §5.5).
    pub fn reconcile_seek(&mut self) -> bool {
        let gen = self.shared.seek_generation();
        if gen == self.generation {
            return true;
        }
        while self.ring.pop().is_ok() {}
        if self.shared.seek_done_generation.load(Ordering::Acquire) == gen {
            self.generation = gen;
            self.shared.set_pos_frames(self.shared.seek_target_frames());
            return true;
        }
        false
    }

    /// Pull up to `data.len()` interleaved samples from the ring. Advances the
    /// position by the consumed frames. Never allocates; returns the number of
    /// samples written.
    pub fn pull_f32(&mut self, data: &mut [f32]) -> usize {
        let out_ch = self.shared.out_ch();
        let avail = self.ring.slots();
        let n = (avail.min(data.len()) / out_ch) * out_ch;
        let mut written = 0usize;
        while written < n {
            match self.ring.pop() {
                Ok(s) => {
                    data[written] = s;
                    written += 1;
                }
                Err(_) => break,
            }
        }
        let frames = written.checked_div(out_ch).unwrap_or(0);
        if frames > 0 {
            self.shared
                .pos_frames
                .fetch_add(frames as u64, Ordering::Relaxed);
        }
        written
    }

    /// Pull up to `len` samples into the internal scratch and return how many
    /// were written. Call [`RtConsumer::scratch`] afterwards to read them.
    pub fn pull_scratch(&mut self, len: usize) -> usize {
        let cap = self.scratch.len();
        let n = len.min(cap);
        if n == 0 {
            return 0;
        }
        let out_ch = self.shared.out_ch();
        let avail = self.ring.slots();
        let take = (avail.min(n) / out_ch) * out_ch;
        let mut written = 0usize;
        while written < take {
            match self.ring.pop() {
                Ok(s) => {
                    self.scratch[written] = s;
                    written += 1;
                }
                Err(_) => break,
            }
        }
        let frames = written.checked_div(out_ch).unwrap_or(0);
        if frames > 0 {
            self.shared
                .pos_frames
                .fetch_add(frames as u64, Ordering::Relaxed);
        }
        written
    }

    /// Read-only view of the scratch filled by [`RtConsumer::pull_scratch`].
    pub fn scratch(&self) -> &[f32] {
        &self.scratch
    }

    /// Last generation reconciled by this consumer (test/diagnostics helper).
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Command sent from the UI thread to the worker (non-real-time channel).
pub enum WorkerCmd {
    /// Seek the decoder and reset the resampler; `generation` echoes the value
    /// returned by [`RtShared::begin_seek`].
    Seek { generation: u64, secs: f64 },
    /// Install/replace the visualizer tap producer on the worker.
    SetVizTap(Option<rtrb::Producer<f32>>),
    /// Terminate the worker loop (the thread also ends when the sender drops).
    Stop,
}

/// Producer thread: owns the decoder and resampler, fills the ring.
pub struct PlaybackWorker {
    handle: Option<JoinHandle<()>>,
    tx: Sender<WorkerCmd>,
    shared: Arc<RtShared>,
}

impl PlaybackWorker {
    /// Spawn the worker for an already-opened source/resampler/ring.
    pub fn spawn(
        source: Box<dyn AudioSource>,
        resampler: Resampler,
        producer: rtrb::Producer<f32>,
        shared: Arc<RtShared>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<WorkerCmd>();
        let loop_shared = shared.clone();
        let handle = thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || worker_loop(source, resampler, producer, loop_shared, rx))
            .ok();
        Self {
            handle,
            tx,
            shared,
        }
    }

    /// Queue a command; a dead worker simply drops it.
    pub fn send(&self, cmd: WorkerCmd) {
        let _ = self.tx.send(cmd);
    }

    /// Ask the worker to stop and wait for the thread to finish.
    pub fn stop(&mut self) {
        self.shared.request_stop();
        let _ = self.tx.send(WorkerCmd::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PlaybackWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Apply a command; returns `true` when the worker must terminate.
fn handle_cmd(
    cmd: WorkerCmd,
    source: &mut Box<dyn AudioSource>,
    resampler: &mut Resampler,
    shared: &RtShared,
    viz_tap: &mut Option<rtrb::Producer<f32>>,
) -> bool {
    match cmd {
        WorkerCmd::Seek { generation, secs } => {
            let _ = source.seek(secs.max(0.0));
            resampler.reset();
            shared.set_eof(false);
            shared.set_finished(false);
            shared.set_natural_end(false);
            // Release pairs with the consumer's Acquire load in reconcile_seek.
            shared.publish_seek_done(generation);
            false
        }
        WorkerCmd::SetVizTap(tap) => {
            *viz_tap = tap;
            false
        }
        WorkerCmd::Stop => true,
    }
}

fn worker_loop(
    mut source: Box<dyn AudioSource>,
    mut resampler: Resampler,
    mut producer: rtrb::Producer<f32>,
    shared: Arc<RtShared>,
    rx: Receiver<WorkerCmd>,
) {
    let out_ch = shared.out_ch();
    if out_ch == 0 {
        shared.rt_err.store(1, Ordering::Relaxed);
        return;
    }
    let mut staging = vec![0.0f32; WORKER_CHUNK_FRAMES * out_ch];
    let mut viz_tap: Option<rtrb::Producer<f32>> = None;

    loop {
        if shared.stop_requested() {
            return;
        }
        // 1. Drain pending commands (non-blocking).
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    if handle_cmd(cmd, &mut source, &mut resampler, &shared, &mut viz_tap) {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // 2. Idle when paused/stopped: block on the command channel.
        if !shared.is_playing() {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(cmd) => {
                    if handle_cmd(cmd, &mut source, &mut resampler, &shared, &mut viz_tap) {
                        return;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        // 3. Produce one chunk from decoder + resampler.
        let produced_frames = produce_chunk(&mut source, &mut resampler, &mut staging, out_ch);
        if produced_frames == 0 {
            if source.eof() {
                shared.set_eof(true);
            }
            // Nothing to push: back off briefly to avoid a busy spin.
            thread::sleep(Duration::from_millis(2));
            continue;
        }

        // 4. Mirror post-resampler PCM into the visualizer tap (pre-volume,
        //    WYSIWYG), then push the chunk into the ring.
        let samples = produced_frames * out_ch;
        if shared.viz_tap_active() {
            if let Some(tap) = viz_tap.as_mut() {
                for &s in staging.iter().take(samples) {
                    if tap.push(s).is_err() {
                        break;
                    }
                }
            }
        }
        push_all(&mut producer, &staging[..samples], &shared);
    }
}

/// Fill `staging` with up to `WORKER_CHUNK_FRAMES` output frames. Returns the
/// number of frames written (0 at end-of-stream once the tail is drained).
fn produce_chunk(
    source: &mut Box<dyn AudioSource>,
    resampler: &mut Resampler,
    staging: &mut [f32],
    out_ch: usize,
) -> usize {
    let max_frames = staging.len() / out_ch;
    let mut produced = 0usize;
    loop {
        if produced >= max_frames {
            break;
        }
        // Keep at least two source frames buffered for interpolation.
        let mut guard = 0u32;
        while resampler.buffered_frames() < 2 && !source.eof() && guard < 1024 {
            guard += 1;
            let Some(s) = source.next_frames() else { break };
            if resampler.push(s) == 0 {
                break;
            }
        }
        let n = resampler.pull(
            &mut staging[produced * out_ch..],
            max_frames - produced,
            source.eof(),
        );
        if n == 0 {
            break;
        }
        produced += n;
    }
    produced
}

/// Push the whole buffer, yielding while the ring is full but the consumer may
/// be playing. Stops early if playback is paused/stopped (stale tail is dropped
/// by the next seek anyway).
fn push_all(producer: &mut rtrb::Producer<f32>, samples: &[f32], shared: &RtShared) {
    let mut i = 0usize;
    while i < samples.len() {
        if shared.stop_requested() {
            return;
        }
        match producer.push(samples[i]) {
            Ok(()) => i += 1,
            Err(_) => {
                if !shared.is_playing() {
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decoder::TrackInfo;
    use std::time::Instant;

    fn ring(capacity: usize) -> (rtrb::Producer<f32>, rtrb::Consumer<f32>) {
        rtrb::RingBuffer::new(capacity)
    }

    /// Deterministic mono source of constant amplitude.
    struct MockSource {
        info: TrackInfo,
        remaining: usize,
        scratch: Vec<f32>,
        eof: bool,
    }

    impl MockSource {
        fn new(frames: usize) -> Self {
            Self {
                info: TrackInfo {
                    sample_rate: 44100,
                    channels: 1,
                    num_frames: Some(frames as u64),
                    format_name: "mock".into(),
                    bitrate: 0,
                    bits: Some(16),
                    tags: Default::default(),
                },
                remaining: frames,
                scratch: vec![0.5f32; 512],
                eof: false,
            }
        }
    }

    impl AudioSource for MockSource {
        fn next_frames(&mut self) -> Option<&[f32]> {
            if self.remaining == 0 {
                self.eof = true;
                return None;
            }
            let n = self.scratch.len().min(self.remaining);
            self.remaining -= n;
            if self.remaining == 0 {
                self.eof = true;
            }
            Some(&self.scratch[..n])
        }
        fn info(&self) -> &TrackInfo {
            &self.info
        }
        fn eof(&self) -> bool {
            self.eof
        }
    }

    #[test]
    fn worker_fills_ring_with_exact_frames() {
        let frames = 2000usize;
        let (prod, cons) = ring(4096);
        let shared = RtShared::new(44100, 1, false);
        let mut consumer = RtConsumer::new(cons, shared.clone());
        let worker = PlaybackWorker::spawn(
            Box::new(MockSource::new(frames)),
            Resampler::new(44100, 44100, 1, 1),
            prod,
            shared.clone(),
        );
        shared.set_finished(false);
        shared.set_playing(true);

        let deadline = Instant::now() + Duration::from_secs(2);
        while !shared.eof() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(shared.eof(), "worker must signal eof");

        let mut out = vec![0.0f32; 4096];
        let mut total = 0usize;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let n = consumer.pull_f32(&mut out);
            total += n;
            if n == 0 {
                break;
            }
        }
        assert_eq!(total, frames, "ring must carry exactly the source frames");
        assert_eq!(shared.pos_frames(), frames as u64);
        drop(worker);
    }

    #[test]
    fn worker_seek_resets_ring_and_position() {
        let (prod, cons) = ring(4096);
        let shared = RtShared::new(44100, 1, false);
        let mut consumer = RtConsumer::new(cons, shared.clone());
        let worker = PlaybackWorker::spawn(
            Box::new(MockSource::new(50_000)),
            Resampler::new(44100, 44100, 1, 1),
            prod,
            shared.clone(),
        );
        shared.set_finished(false);
        shared.set_playing(true);
        thread::sleep(Duration::from_millis(20));

        let gen = shared.begin_seek(44100);
        worker.send(WorkerCmd::Seek {
            generation: gen,
            secs: 1.0,
        });
        // Wait for the worker ack, then reconcile and read new frames.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if consumer.reconcile_seek() {
                break;
            }
            assert!(Instant::now() < deadline, "seek ack timed out");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(shared.pos_frames(), 44100);
        let mut out = vec![0.0f32; 256];
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = 0usize;
        while got == 0 && Instant::now() < deadline {
            got = consumer.pull_f32(&mut out);
            if got == 0 {
                thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(got > 0, "worker must resume producing after the seek");
        drop(worker);
    }

    #[test]
    fn consumer_pulls_only_whole_frames() {
        let (mut prod, cons) = ring(64);
        let shared = RtShared::new(48000, 2, false);
        let mut consumer = RtConsumer::new(cons, shared.clone());
        for i in 0..8 {
            prod.push(i as f32).unwrap();
        }
        let mut out = [0.0f32; 8];
        let n = consumer.pull_f32(&mut out);
        assert_eq!(n, 8);
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(shared.pos_frames(), 4);
    }

    #[test]
    fn consumer_never_tears_a_partial_frame() {
        let (mut prod, cons) = ring(64);
        let shared = RtShared::new(48000, 2, false);
        let mut consumer = RtConsumer::new(cons, shared.clone());
        for i in 0..3 {
            prod.push(i as f32).unwrap();
        }
        let mut out = [0.0f32; 8];
        let n = consumer.pull_f32(&mut out);
        // Three mono samples for a stereo stream: only one whole frame is safe.
        assert_eq!(n, 2);
        assert_eq!(shared.pos_frames(), 1);
    }

    #[test]
    fn scrub_advances_position_by_frames() {
        let (mut prod, cons) = ring(64);
        let shared = RtShared::new(44100, 2, false);
        let mut consumer = RtConsumer::new(cons, shared.clone());
        for i in 0..6 {
            prod.push(i as f32).unwrap();
        }
        let n = consumer.pull_scratch(6);
        assert_eq!(n, 6);
        assert_eq!(consumer.scratch()[..6], [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(shared.pos_frames(), 3);
    }

    #[test]
    fn reconcile_seek_drains_and_rebases() {
        let (mut prod, cons) = ring(64);
        let shared = RtShared::new(48000, 1, false);
        let mut consumer = RtConsumer::new(cons, shared.clone());
        for _ in 0..10 {
            prod.push(0.5).unwrap();
        }
        // Publish a seek to frame 48000 and simulate the worker ack.
        let gen = shared.begin_seek(48000);
        assert!(!consumer.reconcile_seek(), "worker has not acked yet");
        shared.publish_seek_done(gen);
        assert!(consumer.reconcile_seek());
        assert_eq!(shared.pos_frames(), 48000, "position re-based to the target");
        assert_eq!(consumer.generation(), gen);
        // Stale frames were drained.
        let mut out = [0.0f32; 4];
        assert_eq!(consumer.pull_f32(&mut out), 0);
    }

    #[test]
    fn bit_perfect_resampled_tracks_resampler() {
        let shared = RtShared::new(48000, 2, true);
        assert!(!shared.bit_perfect_resampled());
        shared.set_bit_perfect(true);
        assert!(shared.bit_perfect_resampled());
        let identity = RtShared::new(44100, 2, false);
        identity.set_bit_perfect(true);
        assert!(!identity.bit_perfect_resampled());
    }
}
