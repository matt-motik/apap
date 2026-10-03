//! Стадия усиления колбэка (ADR-06, ADR-23, ОВС-18, §6.14).
//!
//! Состояние читается один раз на период; переход между ветвями — только на
//! границе периода, без затухания (как у паузы).

use std::sync::atomic::Ordering;

use crate::audio::session::SessionShared;

/// Ветвь усиления на текущий период (§6.14, таблица «Ветви усиления»).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GainState {
    /// Исходные биты: для `ExactI32` — целочисленный сдвиг, без float и дизеринга (И-Р19).
    Unity,
    /// Линейный коэффициент `f`; дальше TPDF и квантование для целых форматов.
    Scaled(f64),
    /// Тишина формата.
    Muted,
}

/// Стадия усиления: что выбрать на период.
pub trait GainStage: Send + 'static {
    /// Есть ли ступень громкости (у `NoGain` — нет, Строгий режим и «Фиксировать 100 %»).
    const HAS_GAIN: bool;

    fn load(shared: &SessionShared) -> GainState;
}

/// Без ступени громкости: только ворота тишины по `muted` — исходные биты
/// или тишина формата (ОВС-18, ADR-23).
pub struct NoGain;

impl GainStage for NoGain {
    const HAS_GAIN: bool = false;

    #[inline]
    fn load(shared: &SessionShared) -> GainState {
        if shared.muted.load(Ordering::Relaxed) {
            GainState::Muted
        } else {
            GainState::Unity
        }
    }
}

/// Программная громкость из `gain_bits`. Громкость 100 % движок пишет точной
/// константой `1.0f32`, и тогда выбирается `Unity` (И-Р19).
pub struct AtomicGain;

impl GainStage for AtomicGain {
    const HAS_GAIN: bool = true;

    #[inline]
    fn load(shared: &SessionShared) -> GainState {
        if shared.muted.load(Ordering::Relaxed) {
            return GainState::Muted;
        }
        let bits = shared.gain_bits.load(Ordering::Relaxed);
        if bits == 1.0f32.to_bits() {
            GainState::Unity
        } else {
            GainState::Scaled(f64::from(f32::from_bits(bits)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_gain_ignores_volume_and_honours_mute() {
        let s = SessionShared::new();
        s.set_gain(0.25);
        assert_eq!(NoGain::load(&s), GainState::Unity);
        s.muted.store(true, Ordering::Relaxed);
        assert_eq!(NoGain::load(&s), GainState::Muted);
    }

    #[test]
    fn atomic_gain_unity_only_for_exact_one() {
        let s = SessionShared::new();
        assert_eq!(AtomicGain::load(&s), GainState::Unity);
        s.set_gain(0.5);
        assert_eq!(AtomicGain::load(&s), GainState::Scaled(0.5));
        s.set_gain(0.999_999_94);
        assert!(matches!(AtomicGain::load(&s), GainState::Scaled(_)));
        s.muted.store(true, Ordering::Relaxed);
        assert_eq!(AtomicGain::load(&s), GainState::Muted);
    }

    #[test]
    fn strict_render_has_no_gain_stage() {
        const { assert!(!NoGain::HAS_GAIN) };
        const { assert!(AtomicGain::HAS_GAIN) };
    }
}
