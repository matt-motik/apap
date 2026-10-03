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
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::decoder::AudioSource;
use super::error::FileError;
use super::format::SampleBlock;
use super::output::Resampler;
use super::render::RingSample;
use super::session::SessionShared;

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

    /// Подтвердить seek вместо воркера — для тестов колбэка.
    #[cfg(test)]
    pub(crate) fn publish_seek_done_for_test(&self, generation: u64) {
        self.publish_seek_done(generation);
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
    /// Фаза DoP-маркера следующего кадра (false — `0x05`, true — `0xFA`).
    /// Её хранит только колбэк, поэтому чередование не рвётся на паузе,
    /// seek и underrun (ADR-12 Б, ТЗ-2).
    dop_phase: bool,
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
            dop_phase: false,
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

    /// Фаза DoP-маркера следующего кадра (ADR-12 Б).
    pub fn dop_phase(&self) -> bool {
        self.dop_phase
    }

    pub fn set_dop_phase(&mut self, phase: bool) {
        self.dop_phase = phase;
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

    /// Pinned capacity of the preallocated scratch pool (test/diagnostics).
    pub fn scratch_capacity(&self) -> usize {
        self.scratch.capacity()
    }

    /// Last generation reconciled by this consumer (test/diagnostics helper).
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Visualizer tap holder shared between the UI (which installs/removes the
/// producer) and the worker (which pushes post-resampler PCM into it). The
/// worker is not a real-time thread, so a short `Mutex` here is acceptable.
pub type VizTap = Arc<Mutex<Option<rtrb::Producer<f32>>>>;

/// Command sent from the UI thread to the worker (non-real-time channel).
pub enum WorkerCmd {
    /// Seek the decoder and reset the resampler; `generation` echoes the value
    /// returned by [`RtShared::begin_seek`].
    Seek { generation: u64, secs: f64 },
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
        viz_tap: VizTap,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<WorkerCmd>();
        let loop_shared = shared.clone();
        let handle = thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || worker_loop(source, resampler, producer, loop_shared, rx, viz_tap))
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
) -> bool {
    match cmd {
        WorkerCmd::Seek { generation, secs } => {
            let _ = source.seek(secs.max(0.0));
            resampler.reset();
            shared.set_eof(false);
            // Release pairs with the consumer's Acquire load in reconcile_seek.
            shared.publish_seek_done(generation);
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
    viz_tap: VizTap,
) {
    let out_ch = shared.out_ch();
    if out_ch == 0 {
        shared.rt_err.store(1, Ordering::Relaxed);
        return;
    }
    let mut staging = vec![0.0f32; WORKER_CHUNK_FRAMES * out_ch];

    loop {
        if shared.stop_requested() {
            return;
        }
        // 1. Drain pending commands (non-blocking).
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    if handle_cmd(cmd, &mut source, &mut resampler, &shared) {
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
                    if handle_cmd(cmd, &mut source, &mut resampler, &shared) {
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
            if let Ok(mut guard) = viz_tap.lock() {
                if let Some(tap) = guard.as_mut() {
                    for &s in staging.iter().take(samples) {
                        if tap.push(s).is_err() {
                            break;
                        }
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

// ---------------------------------------------------------------------------
// Поток `apap-decode` нового тракта (§6.16–§6.18, ADR-03, ADR-13): пишет
// `SampleBlock` источника в типизированный ring колбэка `render`.
// ---------------------------------------------------------------------------

/// Множитель `ExactI32` → `[-1, 1)`: сэмпл выровнен влево в `i32` (§6.10).
const EXACT_TO_UNIT: f64 = 1.0 / 2_147_483_648.0;

/// Стартовое заполнение ring до `SessionReady` (§6.18): `min(50 % ёмкости, 300 мс)`.
pub fn start_fill_frames(capacity_frames: usize, rate: u32) -> usize {
    let ms300 = usize::try_from(u64::from(rate) * 3 / 10).unwrap_or(usize::MAX);
    (capacity_frames / 2).min(ms300)
}

/// Кадр ring → секунды для `AudioSource::seek`.
fn frame_secs(frame: u64, rate: u32) -> f64 {
    if rate == 0 {
        return 0.0;
    }
    frame as f64 / f64::from(rate)
}

/// Сэмпл ring, который можно зеркалить в tap визуализатора (до громкости).
pub trait TapSample: RingSample {
    fn to_tap(self) -> f32;
}

impl TapSample for i32 {
    #[inline]
    fn to_tap(self) -> f32 {
        (f64::from(self) * EXACT_TO_UNIT) as f32
    }
}

impl TapSample for f32 {
    #[inline]
    fn to_tap(self) -> f32 {
        self
    }
}

/// Источник сэмплов ring: декодер и преобразование под тип ring (ADR-03).
pub trait Feed<P: RingSample>: Send {
    /// Дописать в пустой `out` очередную порцию целых кадров.
    /// `Ok(false)` — конец потока; пустой `out` с `Ok(true)` допустим.
    fn refill(&mut self, out: &mut Vec<P>) -> Result<bool, FileError>;

    /// Фаза 2 seek (§6.16): seek демультиплексора на кадр ring `frame`, сброс
    /// декодера, SRC, DSD-фильтра и упаковщика DoP.
    fn seek(&mut self, frame: u64);

    /// Пропущенных повреждённых пакетов (ТЗ-75).
    fn decode_errors(&self) -> u64 {
        0
    }

    /// Ограниченных при округлении lossy сэмплов (ОВС-11).
    fn lossy_clipped(&self) -> u64 {
        0
    }
}

/// `ExactI32` без изменений в ring `i32` (ТЗ-4, ТЗ-5): PCM без SRC и DoP-нагрузка
/// (биты 23..8, маркер ставит `DopRender`, ADR-12).
pub struct ExactFeed {
    source: Box<dyn AudioSource>,
    /// Кадров ring в секунду (для DoP — частота контейнера).
    frame_rate: u32,
}

impl ExactFeed {
    pub fn new(source: Box<dyn AudioSource>, frame_rate: u32) -> Self {
        Self { source, frame_rate }
    }
}

impl Feed<i32> for ExactFeed {
    fn refill(&mut self, out: &mut Vec<i32>) -> Result<bool, FileError> {
        match self.source.next_block()? {
            None => Ok(false),
            Some(SampleBlock::ExactI32 { data, .. }) => {
                out.extend_from_slice(data);
                Ok(true)
            }
            Some(SampleBlock::F32 { .. } | SampleBlock::DsdBytes { .. }) => {
                Err(FileError::Unsupported {
                    codec: "non-ExactI32 block for an i32 ring".into(),
                })
            }
        }
    }

    fn seek(&mut self, frame: u64) {
        // Ошибка seek оставляет источник на прежней позиции: поток продолжается.
        let _ = self.source.seek(frame_secs(frame, self.frame_rate));
    }

    fn decode_errors(&self) -> u64 {
        self.source.decode_errors()
    }

    fn lossy_clipped(&self) -> u64 {
        self.source.lossy_clipped()
    }
}

/// Ring `f32`: блок источника → `f32` → `Resampler` (SRC, смена каналов или
/// проход 1:1). Пакет источника хранится целиком и подаётся частями, поэтому
/// кадры не теряются при заполненном буфере SRC.
pub struct FloatFeed {
    source: Box<dyn AudioSource>,
    resampler: Resampler,
    out_rate: u32,
    src_ch: usize,
    out_ch: usize,
    /// Текущий пакет источника в `f32` и позиция подачи в SRC (в сэмплах).
    src_buf: Vec<f32>,
    src_off: usize,
    src_done: bool,
}

impl FloatFeed {
    pub fn new(
        source: Box<dyn AudioSource>,
        resampler: Resampler,
        out_rate: u32,
        src_ch: usize,
        out_ch: usize,
    ) -> Self {
        Self {
            source,
            resampler,
            out_rate,
            src_ch: src_ch.max(1),
            out_ch: out_ch.max(1),
            src_buf: Vec::with_capacity(WORKER_CHUNK_FRAMES * src_ch.max(1)),
            src_off: 0,
            src_done: false,
        }
    }
}

impl Feed<f32> for FloatFeed {
    fn refill(&mut self, out: &mut Vec<f32>) -> Result<bool, FileError> {
        out.resize(WORKER_CHUNK_FRAMES * self.out_ch, 0.0);
        loop {
            while self.src_off < self.src_buf.len() {
                let taken = self.resampler.push(&self.src_buf[self.src_off..]);
                if taken == 0 {
                    break;
                }
                self.src_off += taken * self.src_ch;
            }
            let n = self.resampler.pull(out, WORKER_CHUNK_FRAMES, self.src_done);
            if n > 0 {
                out.truncate(n * self.out_ch);
                return Ok(true);
            }
            if self.src_done {
                out.clear();
                return Ok(false);
            }
            if self.src_off < self.src_buf.len() {
                // SRC не принял вход и не выдал кадров: повтор на следующей итерации.
                out.clear();
                return Ok(true);
            }
            self.src_buf.clear();
            self.src_off = 0;
            match self.source.next_block()? {
                None => self.src_done = true,
                Some(SampleBlock::F32 { data }) => self.src_buf.extend_from_slice(data),
                Some(SampleBlock::ExactI32 { data, .. }) => self
                    .src_buf
                    .extend(data.iter().map(|&s| (f64::from(s) * EXACT_TO_UNIT) as f32)),
                Some(SampleBlock::DsdBytes { .. }) => {
                    return Err(FileError::Unsupported {
                        codec: "DSD bytes for an f32 ring".into(),
                    })
                }
            }
        }
    }

    fn seek(&mut self, frame: u64) {
        // Ошибка seek оставляет источник на прежней позиции: поток продолжается.
        let _ = self.source.seek(frame_secs(frame, self.out_rate));
        self.resampler.reset();
        self.src_buf.clear();
        self.src_off = 0;
        self.src_done = false;
    }

    fn decode_errors(&self) -> u64 {
        self.source.decode_errors()
    }

    fn lossy_clipped(&self) -> u64 {
        self.source.lossy_clipped()
    }
}

/// Недописанный хвост (§6.17, И-Р6): остаток порции, не поместившийся в ring.
/// Буфер выделен при открытии; сбрасывается только на фазе 2 seek и при stop.
pub struct PendingTail<P> {
    buf: Vec<P>,
    off: usize,
}

impl<P: Copy> PendingTail<P> {
    pub fn with_capacity(samples: usize) -> Self {
        Self {
            buf: Vec::with_capacity(samples),
            off: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.off >= self.buf.len()
    }

    pub fn remaining(&self) -> &[P] {
        self.buf.get(self.off..).unwrap_or(&[])
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.off = 0;
    }

    fn consume(&mut self, samples: usize) {
        self.off = self.off.saturating_add(samples).min(self.buf.len());
    }
}

/// Событие потока декодирования для движка (§6.18).
#[derive(Debug, PartialEq)]
pub enum DecodeEvent {
    /// Стартовое заполнение набрано (или трек короче него): можно запускать вывод.
    Ready,
    /// Поток прерван: `SessionFailed` (ТЗ-87).
    Failed(FileError),
}

/// Итог одной итерации [`DecodeLoop::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Записано кадров в ring (0 — пустая порция источника).
    Wrote(usize),
    /// Ring заполнен: хвост ждёт места.
    RingFull,
    /// Фаза 2 выполнена, ждём `seek_acked`: запись запрещена (§6.16).
    WaitAck,
    /// Конец потока опубликован в `eof_frame`; ждём seek или stop.
    Ended,
}

/// Геометрия сессии для [`DecodeLoop`].
#[derive(Debug, Clone, Copy)]
pub struct DecodeConfig {
    /// Каналов в кадре ring.
    pub channels: usize,
    /// Кадр ring, с которого начинается сессия (`Open { start_frame }`, §6.18).
    pub start_frame: u64,
    /// Кадров до `Ready` ([`start_fill_frames`]).
    pub start_fill: usize,
    /// Ёмкость `PendingTail` в сэмплах: один декодированный пакет.
    pub tail_samples: usize,
}

/// Логика `apap-decode` без потока: одна итерация — [`DecodeLoop::step`].
/// Тесты гоняют её пошагово вместе с `RenderCore::begin` (модель ADR-13).
pub struct DecodeLoop<P: TapSample, F: Feed<P>> {
    feed: F,
    producer: rtrb::Producer<P>,
    shared: Arc<SessionShared>,
    channels: usize,
    tail: PendingTail<P>,
    my_gen: u64,
    wait_ack: bool,
    /// Кадр ring, с которого пишет текущее поколение.
    base_frame: u64,
    /// Кадров записано в ring текущим поколением.
    written_frames: u64,
    /// Источник исчерпан или прерван ошибкой.
    done: bool,
    failed: bool,
    eof_published: bool,
    start_fill: u64,
    ready_sent: bool,
    events: Sender<DecodeEvent>,
    viz_tap: Option<VizTap>,
    decode_errors: Arc<AtomicU64>,
}

impl<P: TapSample, F: Feed<P>> DecodeLoop<P, F> {
    /// `viz_tap = None` у DoP: tap отсутствует (ТЗ-108).
    pub fn new(
        mut feed: F,
        producer: rtrb::Producer<P>,
        shared: Arc<SessionShared>,
        cfg: DecodeConfig,
        events: Sender<DecodeEvent>,
        viz_tap: Option<VizTap>,
    ) -> Self {
        if cfg.start_frame > 0 {
            feed.seek(cfg.start_frame);
        }
        // Колбэка ещё нет: позицию сессии задаёт декодер, дальше её ведёт колбэк.
        shared.pos_frames.store(cfg.start_frame, Ordering::Relaxed);
        let my_gen = shared.seek_requested.load(Ordering::Acquire);
        Self {
            feed,
            producer,
            shared,
            channels: cfg.channels.max(1),
            tail: PendingTail::with_capacity(cfg.tail_samples),
            my_gen,
            wait_ack: false,
            base_frame: cfg.start_frame,
            written_frames: 0,
            done: false,
            failed: false,
            eof_published: false,
            start_fill: u64::try_from(cfg.start_fill).unwrap_or(u64::MAX),
            ready_sent: false,
            events,
            viz_tap,
            decode_errors: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Счётчик `decode_errors` для движка (`DynamicPathState`, ТЗ-75).
    pub fn decode_errors_handle(&self) -> Arc<AtomicU64> {
        self.decode_errors.clone()
    }

    /// Одна итерация: фаза 2 seek, пополнение хвоста, запись в ring (§6.16, §6.17).
    pub fn step(&mut self) -> Step {
        if self.poll_seek() {
            return Step::WaitAck;
        }
        if self.tail.is_empty() && !self.done {
            self.tail.clear();
            match self.feed.refill(&mut self.tail.buf) {
                Ok(true) => {}
                Ok(false) => self.done = true,
                Err(e) => {
                    self.done = true;
                    self.failed = true;
                    let _ = self.events.send(DecodeEvent::Failed(e));
                }
            }
            self.publish_counters();
        }
        if self.tail.remaining().len() < self.channels {
            // Неполный кадр в хвосте не пишется: ring несёт только целые кадры.
            self.tail.clear();
            if self.done {
                self.publish_eof();
                self.mark_ready();
                return Step::Ended;
            }
            return Step::Wrote(0);
        }
        let frames = self.write_tail();
        if frames == 0 {
            return Step::RingFull;
        }
        self.mark_ready();
        Step::Wrote(frames)
    }

    /// Фаза 2 seek (§6.16, ADR-13). `true` — запись в ring сейчас запрещена.
    fn poll_seek(&mut self) -> bool {
        let s = &*self.shared;
        let mut n = s.seek_requested.load(Ordering::Acquire);
        if n != self.my_gen {
            let target = loop {
                let f = s.seek_target_frame.load(Ordering::Relaxed);
                let again = s.seek_requested.load(Ordering::Acquire);
                if again == n {
                    break f;
                }
                n = again;
            };
            self.tail.clear();
            self.feed.seek(target);
            self.base_frame = target;
            self.written_frames = 0;
            self.done = false;
            self.failed = false;
            self.eof_published = false;
            s.eof_frame.store(u64::MAX, Ordering::Release);
            s.ended.store(false, Ordering::Release);
            // Все записи старого поколения упорядочены раньше этой публикации.
            s.decoder_stopped.store(n, Ordering::Release);
            self.my_gen = n;
            self.wait_ack = true;
        }
        if self.wait_ack {
            if s.seek_acked.load(Ordering::Acquire) != self.my_gen {
                return true;
            }
            self.wait_ack = false;
        }
        false
    }

    /// Записать из хвоста столько целых кадров, сколько есть места.
    fn write_tail(&mut self) -> usize {
        let ch = self.channels;
        let frames = (self.producer.slots() / ch).min(self.tail.remaining().len() / ch);
        let n = frames * ch;
        if n == 0 {
            return 0;
        }
        let Ok(chunk) = self.producer.write_chunk_uninit(n) else {
            return 0;
        };
        let src = self.tail.remaining().get(..n).unwrap_or(&[]);
        chunk.fill_from_iter(src.iter().copied());
        self.mirror_tap(src);
        self.tail.consume(n);
        self.written_frames = self
            .written_frames
            .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
        frames
    }

    /// Зеркало записанных сэмплов в tap визуализатора (до громкости).
    fn mirror_tap(&self, samples: &[P]) {
        let Some(tap) = self.viz_tap.as_ref() else {
            return;
        };
        if !self.shared.viz_tap_active.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut guard) = tap.lock() else {
            return;
        };
        if let Some(p) = guard.as_mut() {
            for &s in samples {
                if p.push(s.to_tap()).is_err() {
                    self.shared.mirror_overflow.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    }

    /// Конец трека (§6.17): `eof_frame` после записи последнего кадра.
    fn publish_eof(&mut self) {
        if self.eof_published {
            return;
        }
        self.eof_published = true;
        let eof = self.base_frame.saturating_add(self.written_frames);
        self.shared.eof_frame.store(eof, Ordering::Release);
    }

    fn mark_ready(&mut self) {
        if self.ready_sent || self.failed {
            return;
        }
        if self.done || self.written_frames >= self.start_fill {
            self.ready_sent = true;
            let _ = self.events.send(DecodeEvent::Ready);
        }
    }

    fn publish_counters(&self) {
        self.decode_errors
            .store(self.feed.decode_errors(), Ordering::Relaxed);
        self.shared
            .lossy_clipped
            .store(self.feed.lossy_clipped(), Ordering::Relaxed);
    }
}

/// Поток `apap-decode` (§6.18): крутит [`DecodeLoop`] до stop.
pub struct DecodeWorker {
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    decode_errors: Arc<AtomicU64>,
}

impl DecodeWorker {
    /// Отказ `spawn` → `OpenFailed(Internal(SpawnFailed))` у движка (ТЗ-88).
    pub fn spawn<P, F>(mut lp: DecodeLoop<P, F>) -> std::io::Result<Self>
    where
        P: TapSample,
        F: Feed<P> + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let decode_errors = lp.decode_errors_handle();
        let flag = stop.clone();
        let handle = thread::Builder::new()
            .name("apap-decode".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match lp.step() {
                        Step::Wrote(_) => {}
                        Step::RingFull | Step::WaitAck => thread::sleep(Duration::from_millis(1)),
                        Step::Ended => thread::sleep(Duration::from_millis(5)),
                    }
                }
                // Stop: `PendingTail` уходит вместе с `lp` (§6.17).
            })?;
        Ok(Self {
            handle: Some(handle),
            stop,
            decode_errors,
        })
    }

    /// Пропущенных повреждённых пакетов (ТЗ-75).
    pub fn decode_errors(&self) -> u64 {
        self.decode_errors.load(Ordering::Relaxed)
    }

    /// Остановить поток и дождаться его завершения.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for DecodeWorker {
    fn drop(&mut self) {
        self.stop();
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

    fn no_tap() -> VizTap {
        Arc::new(Mutex::new(None))
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
        fn next_block(
            &mut self,
        ) -> Result<Option<crate::audio::format::SampleBlock<'_>>, crate::audio::error::FileError>
        {
            Ok(self
                .next_frames()
                .map(|data| crate::audio::format::SampleBlock::F32 { data }))
        }
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
            no_tap(),
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
            no_tap(),
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

    // --- Поток `apap-decode` нового тракта (§6.16–§6.18, §7.3) ---------------

    use crate::audio::render::{Period, RenderCore};
    use std::sync::mpsc;

    /// Счётчик кадров моно: сэмпл = номер кадра; `race_at` вызывает `request_seek`
    /// внутри `refill` (после проверки фазы 2 — гонка ADR-13).
    struct CounterFeed {
        pos: u64,
        total: u64,
        chunk: u64,
        calls: usize,
        race_at: Option<(usize, u64, Arc<SessionShared>)>,
        fail: bool,
    }

    impl CounterFeed {
        fn new(total: u64, chunk: u64) -> Self {
            Self { pos: 0, total, chunk, calls: 0, race_at: None, fail: false }
        }
    }

    impl Feed<i32> for CounterFeed {
        fn refill(&mut self, out: &mut Vec<i32>) -> Result<bool, FileError> {
            self.calls += 1;
            if self.fail {
                return Err(FileError::Unsupported { codec: "mock".into() });
            }
            if let Some((at, target, shared)) = &self.race_at {
                if *at == self.calls {
                    shared.request_seek(*target);
                }
            }
            if self.pos >= self.total {
                return Ok(false);
            }
            let end = (self.pos + self.chunk).min(self.total);
            out.extend((self.pos..end).map(|f| f as i32));
            self.pos = end;
            Ok(true)
        }
        fn seek(&mut self, frame: u64) {
            self.pos = frame;
        }
    }

    fn cfg(start_fill: usize) -> DecodeConfig {
        DecodeConfig { channels: 1, start_frame: 0, start_fill, tail_samples: 64 }
    }

    type Rig<F> = (DecodeLoop<i32, F>, RenderCore<i32>, mpsc::Receiver<DecodeEvent>);

    fn rig<F: Feed<i32>>(feed: F, cap: usize, start_fill: usize, shared: Arc<SessionShared>) -> Rig<F> {
        let (p, c) = rtrb::RingBuffer::<i32>::new(cap);
        let (tx, rx) = mpsc::channel();
        let lp = DecodeLoop::new(feed, p, shared.clone(), cfg(start_fill), tx, None);
        (lp, RenderCore::new(c, shared, 1, 0), rx)
    }

    fn drain(core: &mut RenderCore<i32>) -> Vec<i32> {
        let mut v = Vec::new();
        while let Some(chunk) = core.read(usize::MAX) {
            let (a, b) = chunk.as_slices();
            v.extend_from_slice(a);
            v.extend_from_slice(b);
            chunk.commit_all();
        }
        v
    }

    fn assert_counter_from(v: &[i32], first: i32) {
        assert!(!v.is_empty());
        for (i, &x) in v.iter().enumerate() {
            assert_eq!(x, first + i as i32, "разрыв на позиции {i}");
        }
    }

    #[test]
    fn start_fill_is_half_capacity_or_300ms() {
        assert_eq!(start_fill_frames(1 << 20, 48_000), 14_400);
        assert_eq!(start_fill_frames(8_000, 48_000), 4_000);
    }

    #[test]
    fn seek_protocol_rejects_two_phase_race() {
        let shared = Arc::new(SessionShared::new());
        let mut feed = CounterFeed::new(u64::MAX, 16);
        feed.race_at = Some((3, 1000, shared.clone()));
        let (mut lp, mut core, _rx) = rig(feed, 256, 0, shared.clone());
        assert_eq!(lp.step(), Step::Wrote(16));
        assert_eq!(lp.step(), Step::Wrote(16));
        // Третий refill запрашивает seek (фаза 1) и пишет старый пакет 32..48.
        assert_eq!(lp.step(), Step::Wrote(16));
        let n = shared.seek_requested.load(Ordering::Acquire);
        // Колбэк не сбрасывает ring, пока декодер не остановился.
        assert_eq!(core.begin(), Period::Silence);
        assert_eq!(core.available_frames(), 48);
        // Фаза 2: декодер сбрасывается и ждёт ack, ничего не пишет.
        assert_eq!(lp.step(), Step::WaitAck);
        assert_eq!(shared.decoder_stopped.load(Ordering::Acquire), n);
        assert_eq!(lp.step(), Step::WaitAck);
        assert_eq!(core.available_frames(), 48);
        // Фаза 3: сброс ring и ack.
        assert_eq!(core.begin(), Period::Silence);
        assert_eq!(shared.seek_acked.load(Ordering::Acquire), n);
        assert_eq!(core.available_frames(), 0);
        for _ in 0..4 {
            assert_eq!(lp.step(), Step::Wrote(16));
        }
        assert_counter_from(&drain(&mut core), 1000);
        assert_eq!(shared.pos_frames.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn seek_protocol_series_acks_only_latest() {
        let shared = Arc::new(SessionShared::new());
        let (mut lp, mut core, _rx) = rig(CounterFeed::new(u64::MAX, 16), 256, 0, shared.clone());
        assert_eq!(lp.step(), Step::Wrote(16));
        let n1 = shared.request_seek(100);
        assert_eq!(lp.step(), Step::WaitAck);
        assert_eq!(shared.decoder_stopped.load(Ordering::Acquire), n1);
        let n2 = shared.request_seek(200);
        // decoder_stopped = n1 != n2: ack n1 не выдаётся никогда.
        assert_eq!(core.begin(), Period::Silence);
        assert_ne!(shared.seek_acked.load(Ordering::Acquire), n1);
        assert_eq!(lp.step(), Step::WaitAck);
        assert_eq!(shared.decoder_stopped.load(Ordering::Acquire), n2);
        assert_eq!(core.begin(), Period::Silence);
        assert_eq!(shared.seek_acked.load(Ordering::Acquire), n2);
        assert_eq!(lp.step(), Step::Wrote(16));
        assert_counter_from(&drain(&mut core), 200);
    }

    #[test]
    fn pending_tail_survives_full_ring() {
        let shared = Arc::new(SessionShared::new());
        let (mut lp, mut core, _rx) = rig(CounterFeed::new(500, 16), 10, 0, shared);
        let mut got = Vec::new();
        loop {
            match lp.step() {
                Step::Ended => break,
                Step::RingFull => got.extend(drain(&mut core)),
                Step::Wrote(_) | Step::WaitAck => {}
            }
        }
        got.extend(drain(&mut core));
        assert_eq!(got.len(), 500);
        assert_counter_from(&got, 0);
    }

    #[test]
    fn eof_frame_published_after_last_write() {
        let shared = Arc::new(SessionShared::new());
        let (mut lp, mut core, _rx) = rig(CounterFeed::new(40, 16), 64, 0, shared.clone());
        assert_eq!(lp.step(), Step::Wrote(16));
        assert_eq!(lp.step(), Step::Wrote(16));
        assert_eq!(lp.step(), Step::Wrote(8));
        assert!(!shared.eof_known());
        assert_eq!(lp.step(), Step::Ended);
        assert_eq!(shared.eof_frame.load(Ordering::Acquire), 40);
        assert_eq!(drain(&mut core).len(), 40);
        // Seek после конца снимает eof и продолжает с цели.
        let n = shared.request_seek(10);
        assert_eq!(lp.step(), Step::WaitAck);
        assert!(!shared.eof_known());
        assert_eq!(core.begin(), Period::Silence);
        assert_eq!(shared.seek_acked.load(Ordering::Acquire), n);
        assert_eq!(lp.step(), Step::Wrote(16));
        assert_counter_from(&drain(&mut core), 10);
    }

    #[test]
    fn ready_after_start_fill_or_short_track() {
        let shared = Arc::new(SessionShared::new());
        let (mut lp, _core, rx) = rig(CounterFeed::new(1000, 16), 256, 20, shared);
        lp.step();
        assert!(rx.try_recv().is_err());
        lp.step();
        assert_eq!(rx.try_recv(), Ok(DecodeEvent::Ready));
        lp.step();
        assert!(rx.try_recv().is_err());

        let shared = Arc::new(SessionShared::new());
        let (mut lp, _core, rx) = rig(CounterFeed::new(5, 16), 256, 20, shared);
        assert_eq!(lp.step(), Step::Wrote(5));
        assert!(rx.try_recv().is_err());
        assert_eq!(lp.step(), Step::Ended);
        assert_eq!(rx.try_recv(), Ok(DecodeEvent::Ready));
    }

    #[test]
    fn feed_error_sends_failed_without_ready() {
        let shared = Arc::new(SessionShared::new());
        let mut feed = CounterFeed::new(100, 16);
        feed.fail = true;
        let (mut lp, _core, rx) = rig(feed, 64, 0, shared);
        assert_eq!(lp.step(), Step::Ended);
        assert!(matches!(rx.try_recv(), Ok(DecodeEvent::Failed(FileError::Unsupported { .. }))));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn exact_feed_rejects_float_blocks() {
        let mut feed = ExactFeed::new(Box::new(MockSource::new(10)), 44_100);
        let mut out = Vec::new();
        assert!(matches!(feed.refill(&mut out), Err(FileError::Unsupported { .. })));
    }

    #[test]
    fn float_feed_passes_all_frames() {
        let mut feed = FloatFeed::new(
            Box::new(MockSource::new(2000)),
            Resampler::new(44_100, 44_100, 1, 1),
            44_100,
            1,
            1,
        );
        let mut out = Vec::new();
        let mut total = 0usize;
        loop {
            out.clear();
            let more = feed.refill(&mut out).unwrap();
            assert!(out.iter().all(|&s| s == 0.5));
            total += out.len();
            if !more {
                break;
            }
        }
        assert_eq!(total, 2000);
    }

    #[test]
    fn decode_worker_fills_ring_and_reports_ready() {
        let shared = Arc::new(SessionShared::new());
        let (p, mut c) = rtrb::RingBuffer::<f32>::new(4096);
        let (tx, rx) = mpsc::channel();
        let feed = FloatFeed::new(
            Box::new(MockSource::new(3000)),
            Resampler::new(44_100, 44_100, 1, 1),
            44_100,
            1,
            1,
        );
        let cfg = DecodeConfig { channels: 1, start_frame: 0, start_fill: 2048, tail_samples: 1024 };
        let mut w = DecodeWorker::spawn(DecodeLoop::new(feed, p, shared.clone(), cfg, tx, None)).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(DecodeEvent::Ready));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !shared.eof_known() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(shared.eof_frame.load(Ordering::Acquire), 3000);
        assert_eq!(c.slots(), 3000);
        assert_eq!(c.pop().unwrap(), 0.5);
        w.stop();
    }
}
