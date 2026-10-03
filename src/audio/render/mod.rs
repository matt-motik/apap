//! Колбэк вывода реального времени: общее ядро PCM- и DoP-рендера (§6.14–§6.17).
//!
//! Код этого каталога выполняется в аудио-потоке: без блокировок, логирования,
//! ввода-вывода и аллокаций (ТЗ-98, ТЗ-102, проверяет `tools/check_rt_imports.py`),
//! без паник и индексации (ТЗ-100, И-Р8, линты ниже, §7.7).

#![deny(
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable
)]

pub mod gain;
pub mod out;
pub mod tpdf;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use rtrb::{chunks::ReadChunk, Consumer};

use super::session::SessionShared;

/// Тип сэмпла в ring (ADR-03): `i32` для `ExactI32`, `f32` для `F32`.
/// `Copy` даёт сброс ring за O(1): `commit_all` не вызывает деструкторов (ТЗ-99).
pub trait RingSample: Copy + Send + 'static {
    const ZERO: Self;
}

impl RingSample for i32 {
    const ZERO: i32 = 0;
}

impl RingSample for f32 {
    const ZERO: f32 = 0.0;
}

/// Стартовый запас колбэка (§6.14):
/// `min(max(2 · period_frames, 50 мс · rate), capacity_frames / 2)`.
pub fn prime_frames(period_frames: usize, rate: u32, capacity_frames: usize) -> usize {
    let two_periods = period_frames.saturating_mul(2);
    let fifty_ms = usize::try_from(rate / 20).unwrap_or(usize::MAX);
    two_periods.max(fifty_ms).min(capacity_frames / 2)
}

/// Что делать в текущем периоде после общих проверок [`RenderCore::begin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// Тишина формата (DoP — DoP-тишина); ring не читается, underrun не считается.
    Silence,
    /// Читать ring и писать кадры.
    Play,
}

/// Общее состояние рендера: consumer ring, разделяемые атомики, стартовый запас.
/// `priming` и `prime_frames` принадлежат колбэку, а не `SessionShared` (§2.9).
pub struct RenderCore<P: RingSample> {
    consumer: Consumer<P>,
    shared: Arc<SessionShared>,
    channels: usize,
    priming: bool,
    prime_frames: usize,
}

impl<P: RingSample> RenderCore<P> {
    /// `priming = true` при создании: это старт трека (§6.14).
    /// `channels == 0` заменяется на 1, чтобы деление на число каналов было определено.
    pub fn new(
        consumer: Consumer<P>,
        shared: Arc<SessionShared>,
        channels: usize,
        prime_frames: usize,
    ) -> Self {
        Self {
            consumer,
            shared,
            channels: channels.max(1),
            priming: true,
            prime_frames,
        }
    }

    pub fn shared(&self) -> &SessionShared {
        &self.shared
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn is_priming(&self) -> bool {
        self.priming
    }

    /// Целых кадров в ring.
    pub fn available_frames(&self) -> usize {
        self.consumer
            .slots()
            .checked_div(self.channels)
            .unwrap_or(0)
    }

    /// Начало периода: heartbeat, фаза 3 seek (§6.16), пауза (§6.17), priming (§6.14).
    pub fn begin(&mut self) -> Period {
        let s = &*self.shared;
        s.heartbeat.fetch_add(1, Ordering::Relaxed);

        let req = s.seek_requested.load(Ordering::Acquire);
        if req != s.seek_acked.load(Ordering::Relaxed) {
            if s.decoder_stopped.load(Ordering::Acquire) == req {
                // Все коммиты старого поколения видны после Acquire выше: сброс O(1) (ТЗ-99).
                let slots = self.consumer.slots();
                if let Ok(chunk) = self.consumer.read_chunk(slots) {
                    chunk.commit_all();
                }
                s.pos_frames
                    .store(s.seek_target_frame.load(Ordering::Relaxed), Ordering::Relaxed);
                s.seek_acked.store(req, Ordering::Release);
                self.priming = true;
            }
            return Period::Silence;
        }

        if !s.playing.load(Ordering::Acquire) {
            return Period::Silence;
        }

        if self.priming {
            if self.available_frames() < self.prime_frames && !s.eof_known() {
                return Period::Silence;
            }
            self.priming = false;
        }
        Period::Play
    }

    /// Блок ring на `min(доступно, need_frames)` целых кадров; `None`, если кадров нет.
    pub fn read(&mut self, need_frames: usize) -> Option<ReadChunk<'_, P>> {
        let frames = self.available_frames().min(need_frames);
        let samples = frames.checked_mul(self.channels)?;
        if samples == 0 {
            return None;
        }
        self.consumer.read_chunk(samples).ok()
    }

    /// Конец периода `Play` (§6.14, §6.17): позиция, underrun при нехватке кадров
    /// до конца трека, `ended` по достижении `eof_frame`.
    pub fn finish(&mut self, written_frames: usize, need_frames: usize) {
        let s = &*self.shared;
        let written = u64::try_from(written_frames).unwrap_or(u64::MAX);
        let pos = s
            .pos_frames
            .load(Ordering::Relaxed)
            .saturating_add(written);
        s.pos_frames.store(pos, Ordering::Relaxed);
        let eof_reached = pos >= s.eof_frame.load(Ordering::Acquire);
        if written_frames < need_frames && !eof_reached {
            s.underruns.fetch_add(1, Ordering::Relaxed);
        }
        if eof_reached {
            s.ended.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use rtrb::{Producer, RingBuffer};

    fn core(cap_samples: usize, channels: usize, prime: usize) -> (Producer<i32>, RenderCore<i32>) {
        let (p, c) = RingBuffer::<i32>::new(cap_samples);
        (p, RenderCore::new(c, Arc::new(SessionShared::new()), channels, prime))
    }

    fn push(p: &mut Producer<i32>, n: usize) {
        for i in 0..n {
            p.push(i as i32).unwrap();
        }
    }

    #[test]
    fn prime_frames_formula() {
        // 2 периода больше 50 мс.
        assert_eq!(prime_frames(4096, 48_000, 1 << 20), 8192);
        // 50 мс больше 2 периодов.
        assert_eq!(prime_frames(256, 48_000, 1 << 20), 2400);
        // Ограничение capacity / 2.
        assert_eq!(prime_frames(4096, 48_000, 4000), 2000);
    }

    #[test]
    fn paused_is_silence_and_counts_heartbeat() {
        let (_p, mut c) = core(64, 2, 0);
        assert_eq!(c.begin(), Period::Silence);
        assert_eq!(c.shared().heartbeat.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn priming_holds_until_prime_frames_then_plays() {
        let (mut p, mut c) = core(64, 2, 8);
        c.shared().playing.store(true, Ordering::Release);
        push(&mut p, 14); // 7 кадров
        assert_eq!(c.begin(), Period::Silence);
        assert!(c.is_priming());
        push(&mut p, 2);
        assert_eq!(c.begin(), Period::Play);
        assert!(!c.is_priming());
    }

    #[test]
    fn priming_ends_when_eof_known() {
        let (mut p, mut c) = core(64, 2, 8);
        c.shared().playing.store(true, Ordering::Release);
        push(&mut p, 2);
        c.shared().eof_frame.store(1, Ordering::Release);
        assert_eq!(c.begin(), Period::Play);
    }

    #[test]
    fn phase3_waits_for_decoder_then_flushes_ring() {
        let (mut p, mut c) = core(64, 2, 0);
        c.shared().playing.store(true, Ordering::Release);
        push(&mut p, 20);
        let n = c.shared().request_seek(1234);
        // Декодер ещё не остановился: тишина, ring не трогается.
        assert_eq!(c.begin(), Period::Silence);
        assert_eq!(c.available_frames(), 10);
        c.shared().decoder_stopped.store(n, Ordering::Release);
        assert_eq!(c.begin(), Period::Silence);
        assert_eq!(c.available_frames(), 0);
        assert_eq!(c.shared().seek_acked.load(Ordering::Acquire), n);
        assert_eq!(c.shared().pos_frames.load(Ordering::Relaxed), 1234);
        assert!(c.is_priming());
    }

    #[test]
    fn phase3_ignores_stale_generation() {
        let (_p, mut c) = core(64, 2, 0);
        let n1 = c.shared().request_seek(10);
        c.shared().request_seek(20);
        c.shared().decoder_stopped.store(n1, Ordering::Release);
        assert_eq!(c.begin(), Period::Silence);
        assert_eq!(c.shared().seek_acked.load(Ordering::Acquire), 0);
    }

    #[test]
    fn read_takes_whole_frames_only() {
        let (mut p, mut c) = core(64, 2, 0);
        push(&mut p, 7); // 3 целых кадра + 1 сэмпл
        let chunk = c.read(10).unwrap();
        assert_eq!(chunk.len(), 6);
        chunk.commit_all();
        assert!(c.read(10).is_none());
    }

    #[test]
    fn finish_counts_underrun_before_eof_only() {
        let (_p, mut c) = core(64, 2, 0);
        c.finish(3, 4);
        assert_eq!(c.shared().underruns.load(Ordering::Relaxed), 1);
        assert_eq!(c.shared().pos_frames.load(Ordering::Relaxed), 3);
        c.shared().eof_frame.store(5, Ordering::Release);
        c.finish(2, 4);
        assert_eq!(c.shared().underruns.load(Ordering::Relaxed), 1);
        assert!(c.shared().ended.load(Ordering::Acquire));
    }
}
