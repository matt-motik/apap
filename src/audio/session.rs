//! Разделяемое состояние одной сессии воспроизведения (§2.9, ADR-13).
//!
//! Только атомики: колбэк вывода читает и пишет их без блокировок (ТЗ-10).
//! `Arc<SessionShared>` держит движок до завершения потока вывода, поэтому
//! колбэк никогда не освобождает последнюю ссылку (И-Р7).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use super::error::StreamFault;
use super::format::BitDepth;

/// Тип полезной нагрузки ring, выбирается при открытии трека (ADR-03, ТЗ-4).
///
/// `ExactI32` — целые сэмплы, выровненные влево в `i32`, `valid_bits` значащих
/// старших бит (§6.10). `F32` — после SRC, DSD→PCM или из float-источника.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingPayload {
    ExactI32 { valid_bits: BitDepth },
    F32,
}

/// Разделяемое состояние сессии (§2.9).
pub struct SessionShared {
    // Рукопожатие seek в три фазы (ADR-13, §6.16):
    /// Целевой кадр; пишет движок `Relaxed` до `seek_requested`.
    pub seek_target_frame: AtomicU64,
    /// Фаза 1: поколение запроса, движок, `Release`.
    pub seek_requested: AtomicU64,
    /// Фаза 2: декодер остановил запись старого поколения, `Release`.
    pub decoder_stopped: AtomicU64,
    /// Фаза 3: колбэк сбросил ring, `Release`.
    pub seek_acked: AtomicU64,
    pub playing: AtomicBool,
    /// `f32::to_bits` линейного коэффициента; читает только `AtomicGain`.
    pub gain_bits: AtomicU32,
    /// `AtomicGain` и ворота тишины `NoGain`/`DopRender` (ОВС-18, ADR-23).
    pub muted: AtomicBool,
    pub pos_frames: AtomicU64,
    /// `u64::MAX`, пока конец трека не известен (§6.17).
    pub eof_frame: AtomicU64,
    pub ended: AtomicBool,
    /// Нехватка данных в ring (колбэк), ТЗ-68.
    pub underruns: AtomicU32,
    /// EPIPE драйвера (поток вывода Exclusive), ТЗ-68.
    pub xruns: AtomicU32,
    /// Пишет декодер: ограничено при округлении lossy (ОВС-11, §6.10).
    pub lossy_clipped: AtomicU64,
    /// Пишет декодер: ограничено после компенсации +6 дБ (ОВС-6б).
    pub dsd_clipped: AtomicU64,
    /// Код `StreamFault`.
    pub fault: AtomicU8,
    /// Счётчик вызовов колбэка.
    pub heartbeat: AtomicU64,
    pub test_active: AtomicBool,
    pub mirror_overflow: AtomicBool,
    pub viz_tap_active: AtomicBool,
}

impl SessionShared {
    /// Новая сессия: на паузе, единичное усиление, конец трека не известен.
    pub fn new() -> Self {
        Self {
            seek_target_frame: AtomicU64::new(0),
            seek_requested: AtomicU64::new(0),
            decoder_stopped: AtomicU64::new(0),
            seek_acked: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            gain_bits: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            pos_frames: AtomicU64::new(0),
            eof_frame: AtomicU64::new(u64::MAX),
            ended: AtomicBool::new(false),
            underruns: AtomicU32::new(0),
            xruns: AtomicU32::new(0),
            lossy_clipped: AtomicU64::new(0),
            dsd_clipped: AtomicU64::new(0),
            fault: AtomicU8::new(StreamFault::None.code()),
            heartbeat: AtomicU64::new(0),
            test_active: AtomicBool::new(false),
            mirror_overflow: AtomicBool::new(false),
            viz_tap_active: AtomicBool::new(false),
        }
    }

    /// Фаза 1 seek (§6.16): цель пишется `Relaxed` до `Release`-инкремента
    /// поколения, поэтому видна после `Acquire`-чтения `seek_requested`.
    /// Возвращает новое поколение.
    pub fn request_seek(&self, target_frame: u64) -> u64 {
        self.seek_target_frame.store(target_frame, Ordering::Relaxed);
        self.seek_requested
            .fetch_add(1, Ordering::Release)
            .wrapping_add(1)
    }

    /// Есть ли неподтверждённый колбэком seek (§6.17: в это время underrun не считается).
    pub fn seek_pending(&self) -> bool {
        self.seek_requested.load(Ordering::Acquire) != self.seek_acked.load(Ordering::Acquire)
    }

    /// Линейный коэффициент усиления для `AtomicGain` (ADR-23).
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    /// Пишет коэффициент, ограничивая его `[0, 1]`; NaN даёт 0.
    pub fn set_gain(&self, gain: f32) {
        let g = if gain.is_nan() { 0.0 } else { gain.clamp(0.0, 1.0) };
        self.gain_bits.store(g.to_bits(), Ordering::Relaxed);
    }

    pub fn fault(&self) -> StreamFault {
        StreamFault::from_code(self.fault.load(Ordering::Acquire))
    }

    pub fn set_fault(&self, fault: StreamFault) {
        self.fault.store(fault.code(), Ordering::Release);
    }

    /// Конец трека известен (§6.17).
    pub fn eof_known(&self) -> bool {
        self.eof_frame.load(Ordering::Acquire) != u64::MAX
    }
}

impl Default for SessionShared {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_defaults() {
        let s = SessionShared::new();
        assert!(!s.playing.load(Ordering::Relaxed));
        assert_eq!(s.gain(), 1.0);
        assert!(!s.eof_known());
        assert!(!s.seek_pending());
        assert_eq!(s.fault(), StreamFault::None);
    }

    #[test]
    fn request_seek_bumps_generation_and_publishes_target() {
        let s = SessionShared::new();
        assert_eq!(s.request_seek(100), 1);
        assert_eq!(s.request_seek(200), 2);
        assert_eq!(s.seek_target_frame.load(Ordering::Relaxed), 200);
        assert!(s.seek_pending());
        s.seek_acked.store(2, Ordering::Release);
        assert!(!s.seek_pending());
    }

    #[test]
    fn set_gain_clamps_and_rejects_nan() {
        let s = SessionShared::new();
        s.set_gain(2.0);
        assert_eq!(s.gain(), 1.0);
        s.set_gain(-1.0);
        assert_eq!(s.gain(), 0.0);
        s.set_gain(f32::NAN);
        assert_eq!(s.gain(), 0.0);
        s.set_gain(0.25);
        assert_eq!(s.gain(), 0.25);
    }

    #[test]
    fn fault_roundtrip() {
        let s = SessionShared::new();
        s.set_fault(StreamFault::DeviceUnavailable);
        assert_eq!(s.fault(), StreamFault::DeviceUnavailable);
    }
}
