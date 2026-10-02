//! Форматы источника, блоки декодера и форматы выхода (AM1.0 §2.1).
//! Недопустимые значения непредставимы: конструкторы проверяют диапазоны
//! и возвращают `Option` (ТЗ-101).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use smallvec::SmallVec;
use std::num::{NonZeroU32, NonZeroU8};

/// Частота дискретизации в Гц; ноль непредставим.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SampleRate(NonZeroU32);

impl SampleRate {
    pub const fn new(hz: u32) -> Option<SampleRate> {
        match NonZeroU32::new(hz) {
            Some(nz) => Some(SampleRate(nz)),
            None => None,
        }
    }

    pub const fn hz(self) -> u32 {
        self.0.get()
    }

    /// Семейство кратности: 44,1 кГц (кратные 11 025 Гц), 48 кГц (кратные
    /// 4 000 Гц) или прочие.
    pub const fn family(self) -> RateFamily {
        let hz = self.hz();
        if hz.is_multiple_of(11_025) {
            RateFamily::F44k1
        } else if hz.is_multiple_of(4_000) {
            RateFamily::F48k
        } else {
            RateFamily::Other
        }
    }
}

/// Семейство кратности частот.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RateFamily {
    F44k1,
    F48k,
    Other,
}

/// Раскладка каналов по положению, а не по номеру (ТЗ-12).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChannelLayout {
    positions: SmallVec<[ChannelPos; 8]>,
}

impl ChannelLayout {
    /// Раскладка из позиций; пустая и длиннее 255 каналов непредставимы.
    pub fn new(positions: &[ChannelPos]) -> Option<ChannelLayout> {
        if positions.is_empty() || positions.len() > usize::from(u8::MAX) {
            return None;
        }
        Some(ChannelLayout { positions: SmallVec::from_slice(positions) })
    }

    pub fn mono() -> ChannelLayout {
        ChannelLayout { positions: SmallVec::from_slice(&[ChannelPos::Mono]) }
    }

    pub fn stereo() -> ChannelLayout {
        ChannelLayout { positions: SmallVec::from_slice(&[ChannelPos::FL, ChannelPos::FR]) }
    }

    pub fn positions(&self) -> &[ChannelPos] {
        &self.positions
    }

    /// Число каналов (≥ 1 по построению).
    pub fn count(&self) -> NonZeroU8 {
        let n = u8::try_from(self.positions.len()).unwrap_or(u8::MAX);
        NonZeroU8::new(n).unwrap_or(NonZeroU8::MIN)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelPos {
    FL,
    FR,
    FC,
    LFE,
    SL,
    SR,
    BL,
    BR,
    BC,
    Mono,
    Unknown(u8),
}

/// DSD-частота: только 64·44100·2^k, k = 0..=3 (DSD64…DSD512).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DsdRate {
    Dsd64,
    Dsd128,
    Dsd256,
    Dsd512,
}

impl DsdRate {
    pub const ALL: [DsdRate; 4] = [DsdRate::Dsd64, DsdRate::Dsd128, DsdRate::Dsd256, DsdRate::Dsd512];

    /// Частота DSD-битов на канал, Гц.
    pub const fn bit_rate_hz(self) -> u32 {
        const DSD64: u32 = 64 * 44_100;
        match self {
            DsdRate::Dsd64 => DSD64,
            DsdRate::Dsd128 => DSD64 * 2,
            DsdRate::Dsd256 => DSD64 * 4,
            DsdRate::Dsd512 => DSD64 * 8,
        }
    }

    /// DSD-частота по частоте битов; прочие частоты непредставимы.
    pub fn from_bit_rate(hz: u32) -> Option<DsdRate> {
        DsdRate::ALL.into_iter().find(|r| r.bit_rate_hz() == hz)
    }
}

/// Разрядность PCM-источника (1..=32).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct BitDepth(NonZeroU8);

impl BitDepth {
    pub const fn new(bits: u8) -> Option<BitDepth> {
        if bits > 32 {
            return None;
        }
        match NonZeroU8::new(bits) {
            Some(nz) => Some(BitDepth(nz)),
            None => None,
        }
    }

    pub const fn bits(self) -> u8 {
        self.0.get()
    }
}

/// Контейнер открытого файла (ТЗ-74: показывается на этапе «Источник»).
/// ALAC/AAC в M4A — Mp4; «голый» AAC — Adts; Vorbis — Ogg.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Container {
    Flac,
    Wav,
    Aiff,
    Mp4,
    Ogg,
    Mp3,
    Adts,
    Dsf,
    Dff,
    Other,
}

/// Кодек и признак потерь (ТЗ-74).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    Flac,
    Alac,
    Wav,
    Aiff,
    Mp3,
    Aac,
    Vorbis,
    Adpcm,
    Dsf,
    Dff,
    Other,
}

impl Codec {
    /// Кодеки с потерями: bit-perfect только относительно декодированного
    /// потока (поле `SourceFormat::lossy`).
    pub const fn is_lossy(self) -> bool {
        matches!(self, Codec::Mp3 | Codec::Aac | Codec::Vorbis)
    }
}

/// Факты об источнике, полученные из открытого файла (не из тегов плейлиста, ТЗ-74).
#[derive(Clone, PartialEq, Debug)]
pub struct SourceFormat {
    pub container: Container,
    pub codec: Codec,
    /// true для MP3/AAC/Vorbis: bit-perfect только относительно декодированного потока.
    pub lossy: bool,
    pub rate: SampleRate,
    pub layout: ChannelLayout,
    pub kind: SourceKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    /// PCM с известной разрядностью (lossless). Для lossy (MP3/AAC/Vorbis)
    /// bits = 24: выход декодера, округлённый до 24-битных целых (ОВ-36).
    Pcm { bits: BitDepth },
    /// PCM с плавающей точкой (WAV/AIFF float32/float64, ОВС-1): исходный поток — float-значения.
    FloatPcm { double: bool },
    /// DSD; `rate` в `SourceFormat` — частота DSD-битов на канал.
    Dsd { dsd: DsdRate },
}

/// Блок, который декодер отдаёт тракту (заменяет `next_frames -> &[f32]`).
/// Lossless- и lossy-декодеры отдают целые (ОВ-36); float — только float-PCM (ОВС-1).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SampleBlock<'a> {
    /// Целые исходной разрядности, выровненные влево; младшие 32−valid_bits бит = 0.
    /// Lossless: значения файла без изменений. Lossy (MP3/AAC/Vorbis): float-выход
    /// symphonia, округлённый до 24 бит без дизеринга, valid_bits = 24 (§6.10).
    ExactI32 { data: &'a [i32], valid_bits: BitDepth },
    /// Только PCM-файлы с плавающей точкой (ОВС-1). float32 — значения файла без изменений;
    /// float64 сужается декодером до f32 (в пути — `FloatNarrow`, с потерями).
    F32 { data: &'a [f32] },
    /// DSD-байты, чередование по каналам, как в контейнере после нормализации.
    DsdBytes { data: &'a [u8] },
}

/// Формат сэмпла в точке передачи. Исчерпывающий (R-27, ТЗ-101).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputSampleFormat {
    S16Le,
    S24_3Le,
    S24Le,
    S32Le,
    F32,
}

impl OutputSampleFormat {
    /// Число значащих бит поля (16, 24, 24, 32; для F32 — 24 бита мантиссы).
    pub const fn field_bits(self) -> u8 {
        match self {
            OutputSampleFormat::S16Le => 16,
            OutputSampleFormat::S24_3Le | OutputSampleFormat::S24Le | OutputSampleFormat::F32 => 24,
            OutputSampleFormat::S32Le => 32,
        }
    }
}

/// Подмножество форматов Exclusive на Linux (ОВ-32).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExclusiveSampleFormat {
    S16Le,
    S24_3Le,
    S24Le,
    S32Le,
}

impl From<ExclusiveSampleFormat> for OutputSampleFormat {
    fn from(f: ExclusiveSampleFormat) -> OutputSampleFormat {
        match f {
            ExclusiveSampleFormat::S16Le => OutputSampleFormat::S16Le,
            ExclusiveSampleFormat::S24_3Le => OutputSampleFormat::S24_3Le,
            ExclusiveSampleFormat::S24Le => OutputSampleFormat::S24Le,
            ExclusiveSampleFormat::S32Le => OutputSampleFormat::S32Le,
        }
    }
}

/// Форматы, допустимые для DoP: только 24-битное поле и шире (ADR-12).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DopOutFormat {
    S24_3Le,
    S24Le,
    S32Le,
}

impl From<DopOutFormat> for OutputSampleFormat {
    fn from(f: DopOutFormat) -> OutputSampleFormat {
        match f {
            DopOutFormat::S24_3Le => OutputSampleFormat::S24_3Le,
            DopOutFormat::S24Le => OutputSampleFormat::S24Le,
            DopOutFormat::S32Le => OutputSampleFormat::S32Le,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamFormat {
    pub rate: SampleRate,
    pub sample: OutputSampleFormat,
    pub channels: NonZeroU8,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    fn rate(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    #[test]
    fn sample_rate_rejects_zero_and_classifies_family() {
        assert_eq!(SampleRate::new(0), None);
        for hz in [11_025, 22_050, 44_100, 88_200, 176_400, 352_800, 705_600] {
            assert_eq!(rate(hz).family(), RateFamily::F44k1, "{hz}");
        }
        for hz in [8_000, 16_000, 32_000, 48_000, 96_000, 192_000, 384_000, 768_000] {
            assert_eq!(rate(hz).family(), RateFamily::F48k, "{hz}");
        }
        assert_eq!(rate(37_800).family(), RateFamily::Other);
        assert!(rate(44_100) < rate(48_000));
    }

    #[test]
    fn bit_depth_range_is_1_to_32() {
        assert_eq!(BitDepth::new(0), None);
        assert_eq!(BitDepth::new(33), None);
        assert_eq!(BitDepth::new(1).map(BitDepth::bits), Some(1));
        assert_eq!(BitDepth::new(32).map(BitDepth::bits), Some(32));
    }

    #[test]
    fn dsd_rates_are_64x44100_powers_of_two() {
        assert_eq!(DsdRate::Dsd64.bit_rate_hz(), 2_822_400);
        assert_eq!(DsdRate::Dsd512.bit_rate_hz(), 22_579_200);
        for r in DsdRate::ALL {
            assert_eq!(DsdRate::from_bit_rate(r.bit_rate_hz()), Some(r));
        }
        assert_eq!(DsdRate::from_bit_rate(3_072_000), None);
    }

    #[test]
    fn channel_layout_by_position() {
        assert_eq!(ChannelLayout::new(&[]), None);
        assert_eq!(ChannelLayout::stereo().count().get(), 2);
        assert_eq!(ChannelLayout::mono().positions(), &[ChannelPos::Mono]);
        let surround = [ChannelPos::FL, ChannelPos::FR, ChannelPos::FC, ChannelPos::LFE, ChannelPos::BL, ChannelPos::BR];
        let l = ChannelLayout::new(&surround).unwrap();
        assert_eq!(l.count().get(), 6);
        assert_eq!(l.positions()[3], ChannelPos::LFE);
        assert_eq!(ChannelLayout::new(&vec![ChannelPos::Unknown(0); 256]), None);
    }

    #[test]
    fn output_format_field_bits_and_subsets() {
        assert_eq!(OutputSampleFormat::S16Le.field_bits(), 16);
        assert_eq!(OutputSampleFormat::S24_3Le.field_bits(), 24);
        assert_eq!(OutputSampleFormat::S24Le.field_bits(), 24);
        assert_eq!(OutputSampleFormat::S32Le.field_bits(), 32);
        assert_eq!(OutputSampleFormat::F32.field_bits(), 24);
        assert_eq!(OutputSampleFormat::from(ExclusiveSampleFormat::S24_3Le), OutputSampleFormat::S24_3Le);
        // DoP — только поле ≥ 24 бит (ADR-12).
        for f in [DopOutFormat::S24_3Le, DopOutFormat::S24Le, DopOutFormat::S32Le] {
            assert!(OutputSampleFormat::from(f).field_bits() >= 24);
        }
    }

    #[test]
    fn codec_lossy_flag() {
        for c in [Codec::Mp3, Codec::Aac, Codec::Vorbis] {
            assert!(c.is_lossy());
        }
        for c in [Codec::Flac, Codec::Alac, Codec::Wav, Codec::Aiff, Codec::Adpcm, Codec::Dsf, Codec::Dff] {
            assert!(!c.is_lossy());
        }
    }
}
