//! Генераторы тестовых сигналов (AM1.0 §7.1): вход тестов bit-exact (ТЗ-4,
//! ТЗ-5), seek, паузы и SRC. Целые — в формате `SampleBlock::ExactI32`:
//! значение исходной разрядности, выровненное влево, младшие
//! `32 − valid_bits` бит = 0. Все генераторы детерминированы.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::format::BitDepth;

/// 24 бита; проверка диапазона — при компиляции, во время работы паники нет.
const BITS_24: BitDepth = match BitDepth::new(24) {
    Some(b) => b,
    None => panic!("24 входит в 1..=32"),
};

/// Выровнять значение `valid_bits`-битного сэмпла влево в `i32`.
fn left_align(v: i32, valid_bits: BitDepth) -> i32 {
    v << (32 - u32::from(valid_bits.bits()))
}

/// Поток-счётчик: `s[i] = i mod 2¹⁵`, сдвинутый в `valid_bits` (для
/// `valid_bits < 16` — `i mod 2^(valid_bits − 1)`, чтобы значение помещалось).
/// `i` — номер сэмпла в чередовании каналов, поэтому каналы различаются.
pub fn counter(frames: u32, channels: u8, valid_bits: BitDepth) -> Vec<i32> {
    let modulus_bits = valid_bits.bits().saturating_sub(1).min(15);
    let mask = (1u32 << modulus_bits) - 1;
    let total = u64::from(frames) * u64::from(channels);
    (0..total)
        .map(|i| {
            let low = u32::try_from(i & u64::from(mask)).unwrap_or(0);
            left_align(i32::try_from(low).unwrap_or(0), valid_bits)
        })
        .collect()
}

/// Все 65 536 значений 16-битного сэмпла по возрастанию (ТЗ-4).
pub fn all_i16() -> Vec<i32> {
    (i16::MIN..=i16::MAX).map(|v| i32::from(v) << 16).collect()
}

/// Ширина окрестности «все значения с шагом 1» вокруг опорных точек векторов.
pub const NEIGHBORHOOD: i32 = 256;

/// Детерминированный xorshift64* для псевдослучайной части векторов.
struct TestRng(u64);

impl TestRng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Значения `[centre − NEIGHBORHOOD, centre + NEIGHBORHOOD]` внутри `[min, max]`.
fn neighborhood(centre: i64, min: i64, max: i64) -> impl Iterator<Item = i64> {
    let n = i64::from(NEIGHBORHOOD);
    (centre.saturating_sub(n).max(min))..=(centre.saturating_add(n).min(max))
}

/// Число псевдослучайных значений 24-битного вектора (ТЗ-4).
pub const RANDOM_24: usize = 1_000_000;

/// 24-битный тестовый вектор ТЗ-4: все значения с шагом 1 в окрестностях 0,
/// ±2²², `MIN`, `MAX` плюс 10⁶ псевдослучайных; выровнен влево.
pub fn vector_24bit() -> Vec<i32> {
    const MIN: i64 = -(1 << 23);
    const MAX: i64 = (1 << 23) - 1;
    let mut out: Vec<i32> = [0, 1 << 22, -(1 << 22), MIN, MAX]
        .into_iter()
        .flat_map(|c| neighborhood(c, MIN, MAX))
        .filter_map(|v| i32::try_from(v).ok())
        .collect();
    let mut rng = TestRng(0x0123_4567_89AB_CDEF);
    out.extend((0..RANDOM_24).map(|_| {
        let raw = u32::try_from(rng.next_u64() >> 40).unwrap_or(0);
        i32::try_from(i64::from(raw) + MIN).unwrap_or(0)
    }));
    out.into_iter().map(|v| left_align(v, BITS_24)).collect()
}

/// Число псевдослучайных значений 32-битного вектора (ТЗ-5).
pub const RANDOM_32: usize = 100_000;

/// 32-битный тестовый вектор ТЗ-5: окрестности 0, ±2³⁰, `i32::MIN`,
/// `i32::MAX` и псевдослучайные значения с ненулевыми младшими 8 битами.
pub fn vector_32bit() -> Vec<i32> {
    let (min, max) = (i64::from(i32::MIN), i64::from(i32::MAX));
    let mut out: Vec<i32> = [0, 1 << 30, -(1 << 30), min, max]
        .into_iter()
        .flat_map(|c| neighborhood(c, min, max))
        .filter_map(|v| i32::try_from(v).ok())
        .collect();
    let mut rng = TestRng(0xFEDC_BA98_7654_3210);
    out.extend((0..RANDOM_32).map(|_| {
        let raw = u32::try_from(rng.next_u64() >> 32).unwrap_or(1);
        i32::from_ne_bytes((raw | 1).to_ne_bytes())
    }));
    out
}

/// Синус частоты `freq_hz`, амплитуда `amplitude` (линейная, 0..=1), одинаковый
/// во всех каналах, чередование по каналам.
pub fn sine(rate_hz: u32, freq_hz: f64, amplitude: f32, frames: u32, channels: u8) -> Vec<f32> {
    let step = std::f64::consts::TAU * freq_hz / f64::from(rate_hz);
    (0..frames)
        .flat_map(|n| {
            let v = (step * f64::from(n)).sin() * f64::from(amplitude);
            std::iter::repeat_n(narrow(v), usize::from(channels))
        })
        .collect()
}

/// Логарифмический свип от `f0_hz` до `f1_hz` за `frames` кадров, моно.
pub fn sweep(rate_hz: u32, f0_hz: f64, f1_hz: f64, amplitude: f32, frames: u32) -> Vec<f32> {
    let rate = f64::from(rate_hz);
    let duration = f64::from(frames.max(1)) / rate;
    let k = (f1_hz / f0_hz).ln();
    (0..frames)
        .map(|n| {
            let t = f64::from(n) / rate;
            let phase = std::f64::consts::TAU * f0_hz * duration / k * ((t / duration * k).exp() - 1.0);
            narrow(phase.sin() * f64::from(amplitude))
        })
        .collect()
}

/// Сужение f64 → f32 для значений в [-1, 1]: ближайшее представимое.
fn narrow(v: f64) -> f32 {
    let clamped = v.clamp(-1.0, 1.0);
    // `From<f64> for f32` нет; для значения в [-1, 1] `as` даёт ближайшее f32.
    clamped as f32
}

/// `packets` пакетов по `packet_frames` кадров: тишина, кроме последнего кадра
/// каждого пакета, где во всех каналах стоит `value` (выровненный влево
/// `valid_bits`-битный сэмпл). Проверяет, что хвост пакета не теряется при
/// seek, паузе и смене буфера.
pub fn impulse_at_packet_end(packet_frames: u32, packets: u32, channels: u8, value: i32, valid_bits: BitDepth) -> Vec<i32> {
    let aligned = left_align(value, valid_bits);
    (0..packets)
        .flat_map(|_| (0..packet_frames).map(move |f| if f + 1 == packet_frames { aligned } else { 0 }))
        .flat_map(|s| std::iter::repeat_n(s, usize::from(channels)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    fn bits(n: u8) -> BitDepth {
        BitDepth::new(n).unwrap()
    }

    #[test]
    fn counter_wraps_at_2_pow_15_and_is_left_aligned() {
        let s = counter(20_000, 2, bits(24));
        assert_eq!(s.len(), 40_000);
        assert_eq!(s[1], 1 << 8);
        assert_eq!(s[32_767], 32_767 << 8);
        assert_eq!(s[32_768], 0);
        assert!(s.iter().all(|v| v & 0xFF == 0));
        // Узкая разрядность: значение помещается в valid_bits.
        let narrow8 = counter(300, 1, bits(8));
        assert!(narrow8.iter().all(|v| (v >> 24) < (1 << 7)));
    }

    #[test]
    fn all_i16_covers_every_value() {
        let v = all_i16();
        assert_eq!(v.len(), 65_536);
        assert_eq!(v[0], i32::from(i16::MIN) << 16);
        assert_eq!(v[65_535], i32::from(i16::MAX) << 16);
        assert!(v.windows(2).all(|w| w[1] - w[0] == 1 << 16));
    }

    #[test]
    fn vector_24bit_has_extremes_neighbourhoods_and_random_part() {
        let v = vector_24bit();
        assert!(v.len() > RANDOM_24);
        for x in [0, 1 << 22, -(1 << 22), -(1 << 23), (1 << 23) - 1, -(1 << 23) + 1, (1 << 23) - 2] {
            assert!(v.contains(&(x << 8)), "{x}");
        }
        assert!(v.iter().all(|x| x & 0xFF == 0));
        assert_eq!(v, vector_24bit(), "deterministic");
    }

    #[test]
    fn vector_32bit_has_min_max_and_low_bits() {
        let v = vector_32bit();
        assert!(v.contains(&i32::MIN) && v.contains(&i32::MAX) && v.contains(&0));
        assert!(v.iter().filter(|x| *x & 0xFF != 0).count() >= RANDOM_32);
        assert_eq!(v, vector_32bit());
    }

    #[test]
    fn sine_and_sweep_are_bounded() {
        let s = sine(48_000, 1_000.0, 0.5, 480, 2);
        assert_eq!(s.len(), 960);
        assert_eq!(s[0], 0.0);
        assert_eq!(s[0], s[1]);
        let peak = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 1e-3, "{peak}");
        let w = sweep(44_100, 20.0, 20_000.0, 1.0, 44_100);
        assert_eq!(w.len(), 44_100);
        assert!(w.iter().all(|x| x.abs() <= 1.0));
    }

    #[test]
    fn impulse_only_in_last_frame_of_each_packet() {
        let v = impulse_at_packet_end(4, 3, 2, 1, bits(16));
        assert_eq!(v.len(), 24);
        for (i, x) in v.iter().enumerate() {
            let frame = i / 2;
            let expected = if frame % 4 == 3 { 1 << 16 } else { 0 };
            assert_eq!(*x, expected, "sample {i}");
        }
    }
}
