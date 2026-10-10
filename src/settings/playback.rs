//! Опции режимов воспроизведения (AM1.0 §2.2, ТЗ-93…ТЗ-131).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::backend::SharedDeviceId;
use crate::audio::format::SampleRate;

/// Режим вывода (ADR-04, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ModeKind {
    #[default]
    Compatible,
    Optimal,
    Strict,
}

/// Выбор устройства Совместимого режима: системное по умолчанию или именованное
/// Shared-устройство (§2.2).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum SharedDeviceChoice {
    #[default]
    SystemDefault,
    Named(SharedDeviceId),
}

/// ОВ-3. Значение по умолчанию — SameFamily (§2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RateFallbackRule {
    #[default]
    SameFamily,
    Nearest,
    NoDownsample,
}

/// ОВС-7 (заменяет ОВ-8): фильтры SRC по образцу меню ЦАП, все с линейной фазой
/// и подавлением алиасинга ≥ 150 дБ. «Крутой, короткая задержка» и «Медленный,
/// короткая задержка» (минимальная фаза) непредставимы; UI показывает их из
/// дескриптора как NotImplemented (ТЗ-130, И-Т19, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum SrcFilter {
    #[default]
    Steep,
    Slow,
    VerySlow,
}

/// ОВС-6: начало полосы подавления финального фильтра DSD→PCM; по умолчанию
/// 30 кГц (§2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DsdFilter {
    K24,
    #[default]
    K30,
    K50,
}

/// ОВС-13: буфер устройства в Exclusive; 4 периода в буфере (ТЗ-131, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum DeviceBuffer {
    Ms20,
    #[default]
    Ms40,
    Ms100,
    Ms200,
    Ms400,
}

impl DeviceBuffer {
    /// Размер буфера устройства в миллисекундах (ОВС-13, ТЗ-131).
    pub const fn ms(self) -> u16 {
        match self {
            DeviceBuffer::Ms20 => 20,
            DeviceBuffer::Ms40 => 40,
            DeviceBuffer::Ms100 => 100,
            DeviceBuffer::Ms200 => 200,
            DeviceBuffer::Ms400 => 400,
        }
    }

    /// Длительность одного периода: буфер из 4 периодов (ОВС-13).
    pub const fn period_ms(self) -> u16 {
        self.ms() / 4
    }
}

/// ОВ-7: только два значения (§2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Dither {
    #[default]
    Tpdf,
    Off,
}

/// Решение 6: единственное реализуемое значение. «Ремодуляция» непредставима
/// и показывается в UI из дескриптора как NotImplemented (ТЗ-34, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DsdAboveDac {
    #[default]
    ConvertToPcm,
}

/// Громкость в процентах, 0..=100. Конструктор проверяет диапазон (ТЗ-129,
/// И-Т15, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Volume(u8);

impl Volume {
    pub const fn new(v: u8) -> Option<Volume> {
        if v <= 100 {
            Some(Volume(v))
        } else {
            None
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Размер буфера декодирование→вывод, 100..=10 000 мс (матрица §5, ТЗ-129,
/// И-Т15, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BufferMs(u16);

impl BufferMs {
    pub const fn new(v: u16) -> Option<BufferMs> {
        if v >= 100 && v <= 10_000 {
            Some(BufferMs(v))
        } else {
            None
        }
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Опции всех трёх режимов. Значения неактивных режимов сохраняются (ТЗ-93,
/// §2.2). Активный режим не хранится здесь — он снят в `PlaybackState`
/// (ОВС-15).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ModeSettings {
    pub compatible: CompatibleOpts,
    pub optimal: OptimalOpts,
    pub strict: StrictOpts,
}

/// Режим 1 (Совместимый). Только параметры с «Н» в матрице §5 (ТЗ-93, §2.2).
#[derive(Clone, PartialEq, Debug)]
pub struct CompatibleOpts {
    pub device: SharedDeviceChoice,
    /// Принудительная частота вывода — только здесь (ОВ-4, §2.2).
    pub fixed_rate: Option<SampleRate>,
    pub src_filter: SrcFilter,
    pub dither: Dither,
    pub dsd_filter: DsdFilter,
    /// Компенсация уровня DSD→PCM +6 дБ; по умолчанию true (ОВС-6б, ТЗ-129).
    pub dsd_gain_comp: bool,
    pub buffer: BufferMs,
}

impl Default for CompatibleOpts {
    fn default() -> Self {
        CompatibleOpts {
            device: SharedDeviceChoice::default(),
            fixed_rate: None,
            src_filter: SrcFilter::default(),
            dither: Dither::default(),
            dsd_filter: DsdFilter::default(),
            dsd_gain_comp: true,
            buffer: DEFAULT_BUFFER_MS,
        }
    }
}

/// Режим 2 (Оптимальный, §2.2).
///
/// Поле `device` — минимальная замена не существующему пока
/// `crate::audio::backend::HwDeviceId` (§2.3): этот тип в кодовой базе ещё не
/// определён (создаётся отдельным шагом вне вайтлиста этого шага), поэтому
/// здесь используется строковый идентификатор устройства как временная
/// faithful-репрезентация; заменить на `HwDeviceId`, когда тип появится.
#[derive(Clone, PartialEq, Debug)]
pub struct OptimalOpts {
    pub device: Option<String>,
    pub rate_fallback: RateFallbackRule,
    pub src_filter: SrcFilter,
    pub dither: Dither,
    /// «Фиксировать громкость на 100 %» (ОВС-19); по умолчанию false. true →
    /// `ExclusivePcm<NoGain>`: ступени громкости нет, mute — подменой тишиной
    /// (ADR-23).
    pub volume_lock: bool,
    pub dsd_above_dac: DsdAboveDac,
    pub dsd_filter: DsdFilter,
    /// По умолчанию true (ОВС-6б).
    pub dsd_gain_comp: bool,
    pub buffer: BufferMs,
    /// Инвариант И-Р22: `buffer` ≥ 2 × `device_buffer`.
    pub device_buffer: DeviceBuffer,
}

impl Default for OptimalOpts {
    fn default() -> Self {
        OptimalOpts {
            device: None,
            rate_fallback: RateFallbackRule::default(),
            src_filter: SrcFilter::default(),
            dither: Dither::default(),
            volume_lock: false,
            dsd_above_dac: DsdAboveDac::default(),
            dsd_filter: DsdFilter::default(),
            dsd_gain_comp: true,
            buffer: DEFAULT_BUFFER_MS,
            device_buffer: DeviceBuffer::default(),
        }
    }
}

/// Режим 3 (Строгий, §2.2). Громкости, SRC, дизеринга, фиксированной частоты
/// здесь нет — они непредставимы (ТЗ-41, ТЗ-94). Mute Строгого режима — в
/// `PlaybackState` (ОВС-18).
///
/// Поле `device` — та же временная замена `HwDeviceId`, что и в
/// `OptimalOpts` (см. её doc-комментарий).
#[derive(Clone, PartialEq, Debug)]
pub struct StrictOpts {
    pub device: Option<String>,
    pub buffer: BufferMs,
    /// Инвариант И-Р22: `buffer` ≥ 2 × `device_buffer`.
    pub device_buffer: DeviceBuffer,
}

impl Default for StrictOpts {
    fn default() -> Self {
        StrictOpts {
            device: None,
            buffer: DEFAULT_BUFFER_MS,
            device_buffer: DeviceBuffer::default(),
        }
    }
}

/// Буфер декодирование→вывод по умолчанию: 1500 мс (диапазон 100…10 000 мс,
/// матрица §5, ТЗ-129, И-Т15; согласуется с прежним `RING_BUFFER_MS_DEFAULT`
/// в `settings.rs`).
const DEFAULT_BUFFER_MS: BufferMs = BufferMs(1500);

/// Состояние воспроизведения (ОВС-15): активный режим, громкость и mute по
/// режимам. Меняется из главного окна, трея, горячих клавиш; хранится в
/// `state.toml` (задача 02).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct PlaybackState {
    /// Меняется только в главном окне (решение 1 задачи 02). Default:
    /// Compatible (ТЗ-124).
    pub active: ModeKind,
    pub compatible: ModeGain,
    /// Громкость по умолчанию 100 % (ОВ-5).
    pub optimal: ModeGain,
    /// Громкости в Строгом режиме нет (ТЗ-41); есть только mute (ОВС-18).
    pub strict_muted: bool,
}

/// Громкость и mute одного режима (ОВС-15). Default: 100 %, не выключен
/// (ОВ-5, ТЗ-124).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ModeGain {
    pub volume: Volume,
    pub muted: bool,
}

impl Default for ModeGain {
    fn default() -> Self {
        ModeGain {
            volume: Volume(100),
            muted: false,
        }
    }
}

/// Действующая политика одного открытия трека (ADR-04).
#[derive(Clone, Copy, Debug)]
pub enum PathPolicy<'a> {
    Compatible(&'a CompatibleOpts),
    Optimal(&'a OptimalOpts),
    Strict(&'a StrictOpts),
}

impl<'a> PathPolicy<'a> {
    /// Политика активного режима (ОВС-15): `ModeSettings` больше не содержит
    /// `active`.
    pub fn active(settings: &'a ModeSettings, active: ModeKind) -> PathPolicy<'a> {
        match active {
            ModeKind::Compatible => PathPolicy::Compatible(&settings.compatible),
            ModeKind::Optimal => PathPolicy::Optimal(&settings.optimal),
            ModeKind::Strict => PathPolicy::Strict(&settings.strict),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §7.2: пять значений `DeviceBuffer`, по умолчанию 40 мс; период = буфер/4.
    #[test]
    fn device_buffer_values_and_default() {
        assert_eq!(DeviceBuffer::default(), DeviceBuffer::Ms40);
        let values = [
            (DeviceBuffer::Ms20, 20u16),
            (DeviceBuffer::Ms40, 40u16),
            (DeviceBuffer::Ms100, 100u16),
            (DeviceBuffer::Ms200, 200u16),
            (DeviceBuffer::Ms400, 400u16),
        ];
        for (buffer, ms) in values {
            assert_eq!(buffer.ms(), ms);
            assert_eq!(buffer.period_ms(), ms / 4);
        }
    }

    #[test]
    fn volume_rejects_out_of_range() {
        assert_eq!(Volume::new(0).map(Volume::get), Some(0));
        assert_eq!(Volume::new(100).map(Volume::get), Some(100));
        assert_eq!(Volume::new(101), None);
    }

    #[test]
    fn buffer_ms_rejects_out_of_range() {
        assert_eq!(BufferMs::new(100).map(BufferMs::get), Some(100));
        assert_eq!(BufferMs::new(10_000).map(BufferMs::get), Some(10_000));
        assert_eq!(BufferMs::new(99), None);
        assert_eq!(BufferMs::new(10_001), None);
    }

    /// §7.2 `dsd_filter_and_comp_defaults`: по умолчанию 30 кГц и компенсация
    /// включена в Совместимом и Оптимальном; в `StrictOpts` полей нет (ТЗ-128,
    /// ТЗ-129).
    #[test]
    fn dsd_filter_and_comp_defaults() {
        let compatible = CompatibleOpts::default();
        assert_eq!(compatible.dsd_filter, DsdFilter::K30);
        assert!(compatible.dsd_gain_comp);

        let optimal = OptimalOpts::default();
        assert_eq!(optimal.dsd_filter, DsdFilter::K30);
        assert!(optimal.dsd_gain_comp);
    }

    /// §7.2 `src_filter_default_and_min_phase_not_selectable` (первая часть):
    /// по умолчанию `Steep` (ТЗ-130).
    #[test]
    fn src_filter_default_is_steep() {
        assert_eq!(SrcFilter::default(), SrcFilter::Steep);
        assert_eq!(ModeSettings::default().compatible.src_filter, SrcFilter::Steep);
        assert_eq!(ModeSettings::default().optimal.src_filter, SrcFilter::Steep);
    }

    /// `PathPolicy::active` выбирает опции режима, совпадающего с `ModeKind`
    /// (ADR-04).
    #[test]
    fn path_policy_active_matches_mode() {
        let settings = ModeSettings::default();
        assert!(matches!(
            PathPolicy::active(&settings, ModeKind::Compatible),
            PathPolicy::Compatible(opts) if *opts == settings.compatible
        ));
        assert!(matches!(
            PathPolicy::active(&settings, ModeKind::Optimal),
            PathPolicy::Optimal(opts) if *opts == settings.optimal
        ));
        assert!(matches!(
            PathPolicy::active(&settings, ModeKind::Strict),
            PathPolicy::Strict(opts) if *opts == settings.strict
        ));
    }

    /// `PlaybackState::default()` и `ModeGain::default()`: Совместимый режим,
    /// громкость 100 %, без mute (ОВ-5, ТЗ-124).
    #[test]
    fn playback_state_default_is_compatible_full_volume_unmuted() {
        let state = PlaybackState::default();
        assert_eq!(state.active, ModeKind::Compatible);
        assert_eq!(state.compatible, ModeGain { volume: Volume(100), muted: false });
        assert_eq!(state.optimal, ModeGain { volume: Volume(100), muted: false });
        assert!(!state.strict_muted);
    }
}
