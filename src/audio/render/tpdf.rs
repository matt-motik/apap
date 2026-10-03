//! TPDF-дизеринг колбэка (ТЗ-8, ОВ-7, §6.14).
//!
//! xorshift32 с состоянием в рендере, без аллокаций и блокировок:
//! `d = (u₁ − u₂) · 2⁻³²` в единицах LSB, где `u₁, u₂` — два последовательных
//! 32-битных значения в `f64`. Распределение треугольное на (−1, 1) LSB.

/// Фиксированное зерно рендера (§6.14); тесты задают своё через [`Tpdf::with_seed`].
pub const DEFAULT_SEED: u32 = 0x9E37_79B9;

const TWO_POW_MINUS_32: f64 = 1.0 / 4_294_967_296.0;

/// Генератор шума квантования. Выключенный (`Dither::Off`) всегда даёт 0
/// и не продвигает состояние.
#[derive(Debug, Clone)]
pub struct Tpdf {
    state: u32,
    enabled: bool,
}

impl Tpdf {
    /// Включённый генератор с фиксированным зерном.
    pub fn new() -> Self {
        Self::with_seed(DEFAULT_SEED)
    }

    /// Включённый генератор с заданным зерном; нулевое зерно (неподвижная точка
    /// xorshift) заменяется на 1.
    pub fn with_seed(seed: u32) -> Self {
        Self {
            state: seed.max(1),
            enabled: true,
        }
    }

    /// `Dither::Off`: `d = 0` (ТЗ-7).
    pub fn off() -> Self {
        Self {
            state: DEFAULT_SEED,
            enabled: false,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// Шум в LSB для одного сэмпла.
    #[inline]
    pub fn sample(&mut self) -> f64 {
        if !self.enabled {
            return 0.0;
        }
        let u1 = f64::from(self.next_u32());
        let u2 = f64::from(self.next_u32());
        (u1 - u2) * TWO_POW_MINUS_32
    }
}

impl Default for Tpdf {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn off_is_zero() {
        let mut t = Tpdf::off();
        assert!((0..100).all(|_| t.sample() == 0.0));
    }

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Tpdf::with_seed(7);
        let mut b = Tpdf::with_seed(7);
        assert!((0..1000).all(|_| a.sample() == b.sample()));
    }

    #[test]
    fn triangular_on_open_unit_interval() {
        let mut t = Tpdf::new();
        let n = 200_000;
        let (mut sum, mut sq, mut center) = (0.0, 0.0, 0usize);
        for _ in 0..n {
            let d = t.sample();
            assert!(d > -1.0 && d < 1.0);
            sum += d;
            sq += d * d;
            if d.abs() < 0.5 {
                center += 1;
            }
        }
        let mean = sum / n as f64;
        let var = sq / n as f64;
        assert!(mean.abs() < 0.01, "среднее {mean}");
        // Треугольное на (−1, 1): дисперсия 1/6, P(|d| < 0.5) = 0.75.
        assert!((var - 1.0 / 6.0).abs() < 0.01, "дисперсия {var}");
        let p = center as f64 / n as f64;
        assert!((p - 0.75).abs() < 0.01, "P(|d|<0.5) = {p}");
    }

    #[test]
    fn zero_seed_is_not_stuck() {
        let mut t = Tpdf::with_seed(0);
        assert!((0..10).any(|_| t.sample() != 0.0));
    }
}
