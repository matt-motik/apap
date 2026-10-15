//! Преобразования пути сигнала и их дескриптор (AM1.0 §3.3, ТЗ-71, ТЗ-72, ТЗ-73).
//!
//! Дескриптор `impl TransformKind` — единственное место, где логика «знает»
//! виды преобразований (ТЗ-73): вердикт, рекомендации, окно и отчёт работают
//! только через его пять методов. Все `match` исчерпывающие, поэтому новый
//! вариант (например, будущий `Equalizer`) не соберётся без своих веток.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::fmt;

use crate::audio::format::{BitDepth, ChannelLayout, DsdRate, SampleRate};
use crate::audio::verdict::NbpReason;
use crate::settings::params::ParamId;
use crate::settings::playback::{Dither, DsdFilter, ModeKind, SrcFilter, Volume};

/// Факт преобразования на пути сигнала: что сделано, почему и насколько
/// достоверно это известно (§3.3, ТЗ-71).
#[derive(Clone, PartialEq, Debug)]
pub struct Transform {
    pub kind: TransformKind,
    pub cause: Cause,
    pub confidence: Confidence,
}

/// Дополнение разрядности нулями (16→24/32, 24→32); без потерь (§3.3).
/// Поля приватны: `from < to` гарантирует конструктор.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ZeroPad {
    from: BitDepth,
    to: BitDepth,
}

impl ZeroPad {
    /// Дополнение `from → to`; `None`, если `from >= to` (§3.3).
    pub fn new(from: BitDepth, to: BitDepth) -> Option<ZeroPad> {
        (from < to).then_some(ZeroPad { from, to })
    }

    /// Исходная разрядность.
    pub fn from(self) -> BitDepth {
        self.from
    }

    /// Разрядность после дополнения.
    pub fn to(self) -> BitDepth {
        self.to
    }
}

/// Усечение разрядности; с потерями, причина бейджа — `Truncation` (§3.3).
/// Поля приватны: `from > to` гарантирует конструктор.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Truncate {
    from: BitDepth,
    to: BitDepth,
}

impl Truncate {
    /// Усечение `from → to`; `None`, если `from <= to` (§3.3).
    pub fn new(from: BitDepth, to: BitDepth) -> Option<Truncate> {
        (from > to).then_some(Truncate { from, to })
    }

    /// Исходная разрядность.
    pub fn from(self) -> BitDepth {
        self.from
    }

    /// Разрядность после усечения.
    pub fn to(self) -> BitDepth {
        self.to
    }
}

/// Вид преобразования (§3.3, ТЗ-72). Деление на «без потерь» / «с потерями» —
/// термин ТЗ §2; его отдаёт `lossiness()`.
#[derive(Clone, PartialEq, Debug)]
pub enum TransformKind {
    // без потерь (термин ТЗ §2)
    /// 16→24/32, 24→32; конструктор `ZeroPad::new` проверяет `from < to`.
    ZeroPad(ZeroPad),
    /// Моно → 2 канала дублированием (ОВ-2).
    MonoToStereo,
    /// Размещение в бо́льшем числе каналов: первые каналы, остальные — тишина;
    /// `from < to` (ОВС-2).
    ChannelPad { from: u8, to: u8 },
    /// Целое → float без потерь; `bits ≤ 24`.
    IntToFloat { bits: BitDepth },
    /// Упаковка DSD в DoP.
    DopPack { dsd: DsdRate },
    // с потерями
    /// Ресемплинг выбранным фильтром SRC (ОВС-7).
    Resample { from: SampleRate, to: SampleRate, filter: SrcFilter },
    /// Сведение каналов по ITU-R BS.775, без LFE (ОВ-6).
    Downmix { from: ChannelLayout, to: ChannelLayout },
    /// Усечение разрядности; конструктор `Truncate::new` проверяет `from > to`.
    Truncate(Truncate),
    /// float64 → float32 (ОВС-1); причина — `Truncation`.
    FloatNarrow,
    /// Квантование float-источника в целое (ОВС-1); причина — `FloatToInt`.
    FloatToInt { bits: u8 },
    /// 32-битное целое → float (с потерями: мантисса float32 — 24 бита).
    Int32ToFloat,
    /// stop_hz — действующая граница min(filter, fs_out/2); gain_comp — +6 дБ (ОВС-6, ОВС-6б).
    DsdToPcm { dsd: DsdRate, rate: SampleRate, filter: DsdFilter, stop_hz: u32, gain_comp: bool },
    /// Программная громкость стадии AtomicGain (ОВ-5).
    Volume(Volume),
    /// Mute стадии AtomicGain.
    Mute,
    /// Mute подменой тишиной (NoGain, DopRender; ОВС-18): сэмплы не пересчитываются;
    /// без потерь, badge_reason = None, бейдж — ExclusiveBadge::Muted.
    SilenceGate,
    /// Дизеринг после преобразования с потерями с переквантованием (ТЗ-8, ОВ-7).
    Dither(Dither),
}

/// Без потерь / с потерями (термин ТЗ §2, §3.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lossiness {
    Lossless,
    Lossy,
}

/// Почему появилось преобразование (§3.3, ТЗ-71).
#[derive(Clone, PartialEq, Debug)]
pub enum Cause {
    /// Настройка плеера (рекомендация укажет на неё).
    PlayerSetting(ParamId),
    /// Ограничение устройства.
    DeviceLimit,
    /// Драйвер принял другой формат (ТЗ-30).
    DriverAccepted,
    /// Аудиосервер не принимает (ТЗ-22).
    ServerLimit,
    /// Правило режима (например, DSD→PCM в Совместимом).
    ModeRule,
    /// Zero-padding под формат устройства.
    FormatContainer,
}

/// Достоверность факта (§3.3, ТЗ-71).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Confidence {
    /// Подтверждено указанным источником.
    Confirmed(ConfirmedBy),
    /// Предположено (источник подтверждения недоступен).
    Assumed,
}

/// Чем подтверждён факт (§3.3, ТЗ-71).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfirmedBy {
    /// Прочитанные `hw_params` драйвера.
    DriverHwParams,
    /// Тест тракта во время воспроизведения.
    RuntimeTest,
    /// Заголовок файла.
    FileHeader,
    /// Запрос к ОС / аудиосерверу.
    OsQuery,
}

/// Текст преобразования для окна «Путь сигнала» и отчёта (§3.3, ТЗ-72).
///
/// Спецификация тип не определяет; решение шага AM1.0-8.5: минимальный
/// newtype над `String`. Отдельный тип (а не голый `String`) оставляет место
/// для будущей разметки (например, выделения фильтра) без смены сигнатуры
/// `describe()`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Description(String);

impl Description {
    /// Текст описания.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Description {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Дескриптор: единственное место, где логика «знает» виды преобразований (ТЗ-73).
impl TransformKind {
    /// Без потерь или с потерями (§3.3, термин ТЗ §2). `SilenceGate` — без потерь
    /// (ОВС-18): сэмплы не пересчитываются.
    pub fn lossiness(&self) -> Lossiness {
        match self {
            TransformKind::ZeroPad(_)
            | TransformKind::MonoToStereo
            | TransformKind::ChannelPad { .. }
            | TransformKind::IntToFloat { .. }
            | TransformKind::DopPack { .. }
            | TransformKind::SilenceGate => Lossiness::Lossless,
            TransformKind::Resample { .. }
            | TransformKind::Downmix { .. }
            | TransformKind::Truncate(_)
            | TransformKind::FloatNarrow
            | TransformKind::FloatToInt { .. }
            | TransformKind::Int32ToFloat
            | TransformKind::DsdToPcm { .. }
            | TransformKind::Volume(_)
            | TransformKind::Mute
            | TransformKind::Dither(_) => Lossiness::Lossy,
        }
    }

    /// Причина для бейджа; None у преобразований без потерь (§3.3, ADR-06).
    /// `FloatNarrow` даёт `Truncation` (ОВС-1); `SilenceGate` — None, бейдж
    /// показывает «Mute» отдельным состоянием (ОВС-18).
    pub fn badge_reason(&self) -> Option<NbpReason> {
        match self {
            TransformKind::ZeroPad(_)
            | TransformKind::MonoToStereo
            | TransformKind::ChannelPad { .. }
            | TransformKind::IntToFloat { .. }
            | TransformKind::DopPack { .. }
            | TransformKind::SilenceGate => None,
            TransformKind::Resample { .. } => Some(NbpReason::Resample),
            TransformKind::Downmix { .. } => Some(NbpReason::Downmix),
            TransformKind::Truncate(_) | TransformKind::FloatNarrow => Some(NbpReason::Truncation),
            TransformKind::FloatToInt { .. } => Some(NbpReason::FloatToInt),
            TransformKind::Int32ToFloat => Some(NbpReason::Int32ToFloat),
            TransformKind::DsdToPcm { .. } => Some(NbpReason::DsdToPcm),
            TransformKind::Volume(_) => Some(NbpReason::Volume),
            TransformKind::Mute => Some(NbpReason::Mute),
            TransformKind::Dither(_) => Some(NbpReason::Dither),
        }
    }

    /// Разрешено ли преобразование в режиме (§3.3, матрица режимов ТЗ §5).
    ///
    /// Правило: Строгий разрешает только Lossless (вкл. DoP и подмену тишиной,
    /// ОВС-18). В Совместимом нет `DopPack` — Shared не умеет DoP (И-Т3, строка
    /// «DSD: DoP» матрицы); прочее разрешено: при сервере без float там
    /// возможны дополнение нулями, усечение и квантование (ОВС-4). Оптимальный
    /// разрешает всё: каждое преобразование там допустимо при соответствующем
    /// ограничении устройства или настройке (ОВС-1, ОВС-3, ОВС-19).
    pub fn allowed_in(&self, mode: ModeKind) -> bool {
        match mode {
            ModeKind::Strict => self.lossiness() == Lossiness::Lossless,
            ModeKind::Compatible => match self {
                TransformKind::DopPack { .. } => false,
                TransformKind::ZeroPad(_)
                | TransformKind::MonoToStereo
                | TransformKind::ChannelPad { .. }
                | TransformKind::IntToFloat { .. }
                | TransformKind::Resample { .. }
                | TransformKind::Downmix { .. }
                | TransformKind::Truncate(_)
                | TransformKind::FloatNarrow
                | TransformKind::FloatToInt { .. }
                | TransformKind::Int32ToFloat
                | TransformKind::DsdToPcm { .. }
                | TransformKind::Volume(_)
                | TransformKind::Mute
                | TransformKind::SilenceGate
                | TransformKind::Dither(_) => true,
            },
            ModeKind::Optimal => true,
        }
    }

    /// Настройка, которая может убрать или изменить преобразование (для
    /// рекомендаций, §3.3, §3.4). Выбор по строкам матрицы ТЗ §5: у ресемплинга
    /// пользователь управляет фильтром (`SrcFilter`; факт SRC задан режимом);
    /// переходы целое↔float — форматом сэмпла (`SampleFormat`), квантование и
    /// сужение float-источника — строкой `FloatSource` (ОВС-1); mute и подмена
    /// тишиной — `Mute` (ОВС-18).
    pub fn controlling_param(&self) -> Option<ParamId> {
        let id = match self {
            TransformKind::ZeroPad(_) => ParamId::ZeroPad,
            TransformKind::MonoToStereo => ParamId::MonoToStereo,
            TransformKind::ChannelPad { .. } => ParamId::ChannelPad,
            TransformKind::IntToFloat { .. } => ParamId::SampleFormat,
            TransformKind::DopPack { .. } => ParamId::DsdDop,
            TransformKind::Resample { .. } => ParamId::SrcFilter,
            TransformKind::Downmix { .. } => ParamId::Downmix,
            TransformKind::Truncate(_) => ParamId::Truncation,
            TransformKind::FloatNarrow => ParamId::FloatSource,
            TransformKind::FloatToInt { .. } => ParamId::FloatSource,
            TransformKind::Int32ToFloat => ParamId::SampleFormat,
            TransformKind::DsdToPcm { .. } => ParamId::DsdToPcm,
            TransformKind::Volume(_) => ParamId::Volume,
            TransformKind::Mute | TransformKind::SilenceGate => ParamId::Mute,
            TransformKind::Dither(_) => ParamId::DitherKind,
        };
        Some(id)
    }

    /// Русский текст для окна и отчёта (§3.3, ТЗ-72), например
    /// «192 кГц → 96 кГц, SRC, фильтр «Крутой»». Фильтры называются так же,
    /// как в диалоге настроек (§6.11, §6.12).
    pub fn describe(&self) -> Description {
        let text = match self {
            TransformKind::ZeroPad(z) => {
                format!("{} → {} бит, дополнение нулями", z.from().bits(), z.to().bits())
            }
            TransformKind::MonoToStereo => "Моно → 2 канала, дублирование".to_owned(),
            TransformKind::ChannelPad { from, to } => {
                format!("{from} → {to} кан., остальные каналы — тишина")
            }
            TransformKind::IntToFloat { bits } => {
                format!("{} бит → float, без потерь", bits.bits())
            }
            TransformKind::DopPack { dsd } => format!("{}, упаковка в DoP", dsd_name(*dsd)),
            TransformKind::Resample { from, to, filter } => format!(
                "{} → {}, SRC, фильтр «{}»",
                khz(from.hz()),
                khz(to.hz()),
                src_filter_name(*filter)
            ),
            TransformKind::Downmix { from, to } => format!(
                "{} → {} кан., downmix ITU-R BS.775 без LFE",
                from.count().get(),
                to.count().get()
            ),
            TransformKind::Truncate(t) => {
                format!("{} → {} бит, усечение", t.from().bits(), t.to().bits())
            }
            TransformKind::FloatNarrow => "float64 → float32, усечение".to_owned(),
            TransformKind::FloatToInt { bits } => {
                format!("float → {bits} бит, квантование в целое")
            }
            TransformKind::Int32ToFloat => "32 бит → float, с потерями".to_owned(),
            TransformKind::DsdToPcm { dsd, rate, filter, stop_hz, gain_comp } => {
                let comp = if *gain_comp { ", компенсация +6 дБ" } else { "" };
                format!(
                    "{} → PCM {}, фильтр {} (граница {}){}",
                    dsd_name(*dsd),
                    khz(rate.hz()),
                    dsd_filter_name(*filter),
                    khz(*stop_hz),
                    comp
                )
            }
            TransformKind::Volume(v) => format!("Громкость {} %", v.get()),
            TransformKind::Mute => "Mute: усиление 0".to_owned(),
            TransformKind::SilenceGate => "Mute: подмена тишиной, без пересчёта сэмплов".to_owned(),
            TransformKind::Dither(d) => match d {
                Dither::Tpdf => "Дизеринг TPDF".to_owned(),
                Dither::Off => "Переквантование без дизеринга".to_owned(),
            },
        };
        Description(text)
    }
}

/// Частота в кГц с десятичной запятой без лишних нулей: 44 100 → «44,1 кГц»,
/// 96 000 → «96 кГц», 22 050 → «22,05 кГц» (без `as`, целочисленно).
fn khz(hz: u32) -> String {
    let int = hz / 1000;
    let frac = hz % 1000;
    if frac == 0 {
        return format!("{int} кГц");
    }
    let digits = format!("{frac:03}");
    format!("{int},{} кГц", digits.trim_end_matches('0'))
}

/// Имя DSD-частоты для текста (§3.3).
fn dsd_name(dsd: DsdRate) -> &'static str {
    match dsd {
        DsdRate::Dsd64 => "DSD64",
        DsdRate::Dsd128 => "DSD128",
        DsdRate::Dsd256 => "DSD256",
        DsdRate::Dsd512 => "DSD512",
    }
}

/// Название фильтра SRC, как в диалоге настроек (ОВС-7, ТЗ-130).
fn src_filter_name(filter: SrcFilter) -> &'static str {
    match filter {
        SrcFilter::Steep => "Крутой",
        SrcFilter::Slow => "Медленный",
        SrcFilter::VerySlow => "Очень медленный",
    }
}

/// Название фильтра DSD→PCM — начало полосы подавления (ОВС-6, ТЗ-128).
fn dsd_filter_name(filter: DsdFilter) -> &'static str {
    match filter {
        DsdFilter::K24 => "24 кГц",
        DsdFilter::K30 => "30 кГц",
        DsdFilter::K50 => "50 кГц",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    fn bd(bits: u8) -> BitDepth {
        BitDepth::new(bits).unwrap()
    }

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    /// Представитель каждого варианта `TransformKind` (16 штук).
    fn all_kinds() -> Vec<TransformKind> {
        let five_one = ChannelLayout::new(&[
            crate::audio::format::ChannelPos::FL,
            crate::audio::format::ChannelPos::FR,
            crate::audio::format::ChannelPos::FC,
            crate::audio::format::ChannelPos::LFE,
            crate::audio::format::ChannelPos::SL,
            crate::audio::format::ChannelPos::SR,
        ])
        .unwrap();
        vec![
            TransformKind::ZeroPad(ZeroPad::new(bd(16), bd(24)).unwrap()),
            TransformKind::MonoToStereo,
            TransformKind::ChannelPad { from: 2, to: 8 },
            TransformKind::IntToFloat { bits: bd(16) },
            TransformKind::DopPack { dsd: DsdRate::Dsd64 },
            TransformKind::Resample { from: sr(192_000), to: sr(96_000), filter: SrcFilter::Steep },
            TransformKind::Downmix { from: five_one, to: ChannelLayout::stereo() },
            TransformKind::Truncate(Truncate::new(bd(32), bd(24)).unwrap()),
            TransformKind::FloatNarrow,
            TransformKind::FloatToInt { bits: 24 },
            TransformKind::Int32ToFloat,
            TransformKind::DsdToPcm {
                dsd: DsdRate::Dsd64,
                rate: sr(88_200),
                filter: DsdFilter::K30,
                stop_hz: 30_000,
                gain_comp: true,
            },
            TransformKind::Volume(Volume::new(50).unwrap()),
            TransformKind::Mute,
            TransformKind::SilenceGate,
            TransformKind::Dither(Dither::Tpdf),
        ]
    }

    /// ТЗ-73, §7: дескриптор тотален; тексты непусты, причина бейджа есть
    /// ровно у преобразований с потерями, Строгий пускает только Lossless.
    #[test]
    fn transform_descriptor_is_total() {
        let kinds = all_kinds();
        assert_eq!(kinds.len(), 16);
        for k in &kinds {
            assert!(!k.describe().as_str().is_empty(), "{k:?}");
            assert_eq!(k.badge_reason().is_some(), k.lossiness() == Lossiness::Lossy, "{k:?}");
            assert_eq!(k.allowed_in(ModeKind::Strict), k.lossiness() == Lossiness::Lossless, "{k:?}");
            assert!(k.allowed_in(ModeKind::Optimal), "{k:?}");
            assert!(k.controlling_param().is_some(), "{k:?}");
        }
        let dop = TransformKind::DopPack { dsd: DsdRate::Dsd64 };
        assert!(!dop.allowed_in(ModeKind::Compatible));
        assert_eq!(
            kinds[5].describe().to_string(),
            "192 кГц → 96 кГц, SRC, фильтр «Крутой»"
        );
        assert_eq!(kinds[0].describe().as_str(), "16 → 24 бит, дополнение нулями");
        assert_eq!(khz(44_100), "44,1 кГц");
        assert_eq!(khz(22_050), "22,05 кГц");
    }

    /// §3.3: конструкторы проверяют порядок разрядностей.
    #[test]
    fn zero_pad_and_truncate_constructors_check_order() {
        let z = ZeroPad::new(bd(16), bd(24)).unwrap();
        assert_eq!((z.from(), z.to()), (bd(16), bd(24)));
        assert!(ZeroPad::new(bd(24), bd(24)).is_none());
        assert!(ZeroPad::new(bd(24), bd(16)).is_none());

        let t = Truncate::new(bd(32), bd(24)).unwrap();
        assert_eq!((t.from(), t.to()), (bd(32), bd(24)));
        assert!(Truncate::new(bd(24), bd(24)).is_none());
        assert!(Truncate::new(bd(16), bd(24)).is_none());
    }
}
