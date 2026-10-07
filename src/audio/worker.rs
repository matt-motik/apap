//! Поток `apap-decode` (§6.16–§6.18, ADR-03, ADR-13): декодирование и ресемплинг
//! вне RT-потока; `SampleBlock` источника пишется в типизированный SPSC-ring,
//! который читает колбэк `render`. Общее состояние — атомики `SessionShared`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::decoder::AudioSource;
use super::error::{EngineFault, FileError};
use super::format::SampleBlock;
use super::output::Resampler;
use super::render::RingSample;
use super::session::SessionShared;
use crate::engine::spawner::ThreadSpawner;

/// Кадров выхода за один `FloatFeed::refill`: малая латентность seek при
/// амортизации обращений к ring.
const WORKER_CHUNK_FRAMES: usize = 1024;

/// Visualizer tap holder shared between the UI (which installs/removes the
/// producer) and the worker (which pushes post-resampler PCM into it). The
/// worker is not a real-time thread, so a short `Mutex` here is acceptable.
pub type VizTap = Arc<Mutex<Option<rtrb::Producer<f32>>>>;

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
    /// Отказ `spawn` → `EngineFault::SpawnFailed` (ТЗ-88, ADR-20); у движка —
    /// `OpenFailed(Internal(SpawnFailed))`.
    pub fn spawn<P, F>(spawner: &dyn ThreadSpawner, mut lp: DecodeLoop<P, F>) -> Result<Self, EngineFault>
    where
        P: TapSample,
        F: Feed<P> + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let decode_errors = lp.decode_errors_handle();
        let flag = stop.clone();
        let job = Box::new(move || {
            while !flag.load(Ordering::Relaxed) {
                match lp.step() {
                    Step::Wrote(_) => {}
                    Step::RingFull | Step::WaitAck => thread::sleep(Duration::from_millis(1)),
                    Step::Ended => thread::sleep(Duration::from_millis(5)),
                }
            }
            // Stop: `PendingTail` уходит вместе с `lp` (§6.17).
        });
        let handle = spawner.spawn("apap-decode", job)?;
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
    use crate::engine::spawner::{FailingSpawner, StdSpawner};
    use std::time::Instant;

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
            if self.remaining == 0 {
                self.eof = true;
                return Ok(None);
            }
            let n = self.scratch.len().min(self.remaining);
            self.remaining -= n;
            if self.remaining == 0 {
                self.eof = true;
            }
            Ok(Some(crate::audio::format::SampleBlock::F32 { data: &self.scratch[..n] }))
        }
        fn info(&self) -> &TrackInfo {
            &self.info
        }
        fn eof(&self) -> bool {
            self.eof
        }
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
        let mut w = DecodeWorker::spawn(&StdSpawner, DecodeLoop::new(feed, p, shared.clone(), cfg, tx, None))
            .unwrap();
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

    #[test]
    fn decode_worker_spawn_failure_is_reported() {
        let shared = Arc::new(SessionShared::new());
        let (p, _c) = rtrb::RingBuffer::<f32>::new(4096);
        let (tx, _rx) = mpsc::channel();
        let feed = FloatFeed::new(
            Box::new(MockSource::new(3000)),
            Resampler::new(44_100, 44_100, 1, 1),
            44_100,
            1,
            1,
        );
        let cfg = DecodeConfig { channels: 1, start_frame: 0, start_fill: 2048, tail_samples: 1024 };
        let spawner = FailingSpawner::new(1);
        let result = DecodeWorker::spawn(&spawner, DecodeLoop::new(feed, p, shared, cfg, tx, None));
        assert!(matches!(
            result,
            Err(EngineFault::SpawnFailed { what: "apap-decode" })
        ));
    }
}
