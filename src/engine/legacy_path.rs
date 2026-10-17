//! Адаптер новой модели режимов (ADR-04, §2.2) к старому аудио-пути
//! `LegacyAudio` (ОВС-12, §8 С4). Старый путь остаётся единственным
//! исполнителем вывода на Linux до замены в С6; этот модуль переводит
//! `ModeSettings` активного режима в параметры, которые старый путь уже
//! умеет применять (`EngineCmd::SetLegacyAudio` / `EngineCmd::SetDevice`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::num::NonZeroU8;

use smallvec::SmallVec;

use crate::audio::backend::{AccessKind, DeviceCaps, DriverFormat, OutputApi, RateSet};
use crate::audio::format::{OutputSampleFormat, SampleRate, StreamFormat};
use crate::audio::path::{OpenedFacts, OsStage, ReservationView};
use crate::audio::player::StreamDesc;
use crate::engine::messages::LegacyAudio;
use crate::settings::playback::{
    CompatibleOpts, Dither, ModeKind, ModeSettings, OptimalOpts, RateFallbackRule,
    SharedDeviceChoice, SrcFilter, StrictOpts,
};
use crate::settings::{
    clamp_ring_buffer_ms, ClockFamily, DsdMode, ExclusiveMode, FallbackPolicy, FallbackRatePolicy,
    ResamplerAlgorithm, ResamplerDither, ResamplerMode,
};

/// Параметры старого пути для активного режима (ОВС-12, §8 С4): `LegacyAudio`
/// для `EngineCmd::SetLegacyAudio` и устройство для `EngineCmd::SetDevice`
/// (`None` — системный дефолт/без смены устройства).
///
/// Семантика по режимам (старые перечисления — только внутренние параметры
/// этого пути, в файле настроек не представимы):
/// - Совместимый: `ExclusiveMode::Off` (shared-путь); DSD конвертируется в
///   PCM (`DsdMode::Pcm`, решение 6 — единственное реализуемое значение).
///   `fixed_rate: Some(hz)` → `ResamplerMode::Fixed` с этой частотой,
///   `None` → `ResamplerMode::Auto` (ресемплинг только если native-частота
///   недоступна — ближайший старый аналог «следовать за источником»).
/// - Оптимальный: `ExclusiveMode::Strict`, а не `ExclusiveMode::Auto` —
///   `Auto` при неудаче один раз откатывается в shared, а решение 4
///   (ОВС-12) требует отказа без перехода в Shared. `resampler_mode: Auto` +
///   `fallback_policy: Nearest` — единственная комбинация старого пути, где
///   при недоступности native rate используется именно `fallback_rate`
///   (`FallbackRatePolicy`), на который отображается `RateFallbackRule`.
/// - Строгий: `ResamplerMode::Native` (exact match или `Err`, без SRC),
///   `fallback_policy: Fail` (без фолбека по каналам), `dither: Off`,
///   `bit_perfect: true`.
///
/// `SrcFilter` (крутизна АЧХ фильтра DAC-меню, §2.2) не имеет старого
/// аналога по назначению — отображается на ближайший по крутизне
/// `ResamplerAlgorithm` (больше тапов windowed-sinc ⇒ круче срез):
/// `Steep → SincSlow`, `Slow → SincMedium`, `VerySlow → SincFast`
/// (решение шага, отдельного ОВС под это нет).
///
/// Поля без старого аналога отбрасываются молча — старый путь не умеет их
/// применить: `DsdFilter`/`dsd_gain_comp` (DSD→PCM фильтр и компенсация
/// уровня — старый CIC-путь их не параметризует), `volume_lock`
/// (громкость/mute не часть `LegacyAudio`, см. `src/app/mod.rs`),
/// `device_buffer` (`LegacyAudio` не содержит отдельного буфера устройства —
/// единственный буферный параметр старого пути — `ring_buffer_ms`).
pub fn legacy_audio(settings: &ModeSettings, active: ModeKind) -> (LegacyAudio, Option<String>) {
    match active {
        ModeKind::Compatible => compatible_legacy_audio(&settings.compatible),
        ModeKind::Optimal => optimal_legacy_audio(&settings.optimal),
        ModeKind::Strict => strict_legacy_audio(&settings.strict),
    }
}

/// `SrcFilter` → `ResamplerAlgorithm` (см. doc-комментарий `legacy_audio`).
fn src_filter_to_algorithm(filter: SrcFilter) -> ResamplerAlgorithm {
    match filter {
        SrcFilter::Steep => ResamplerAlgorithm::SincSlow,
        SrcFilter::Slow => ResamplerAlgorithm::SincMedium,
        SrcFilter::VerySlow => ResamplerAlgorithm::SincFast,
    }
}

/// `RateFallbackRule` → `FallbackRatePolicy`: совпадение по смыслу
/// (ближайшая/в семействе/без даунсемплинга, ОВ-3).
fn rate_fallback_to_legacy(rule: RateFallbackRule) -> FallbackRatePolicy {
    match rule {
        RateFallbackRule::SameFamily => FallbackRatePolicy::SameFamily,
        RateFallbackRule::Nearest => FallbackRatePolicy::Nearest,
        RateFallbackRule::NoDownsample => FallbackRatePolicy::NeverDownsample,
    }
}

/// `Dither` (ОВ-7, только Tpdf/Off) → `ResamplerDither`.
fn dither_to_legacy(dither: Dither) -> ResamplerDither {
    match dither {
        Dither::Tpdf => ResamplerDither::Tpdf,
        Dither::Off => ResamplerDither::Off,
    }
}

fn compatible_legacy_audio(opts: &CompatibleOpts) -> (LegacyAudio, Option<String>) {
    let (resampler_mode, fixed_rate) = match opts.fixed_rate {
        Some(rate) => (ResamplerMode::Fixed, rate.hz()),
        None => (ResamplerMode::Auto, 0),
    };
    let device = match &opts.device {
        SharedDeviceChoice::SystemDefault => None,
        SharedDeviceChoice::Named(id) => Some(id.as_str().to_string()),
    };
    let legacy = LegacyAudio {
        exclusive_mode: ExclusiveMode::Off,
        fallback_policy: FallbackPolicy::Nearest,
        dsd_mode: DsdMode::Pcm,
        resampler_mode,
        resampler_algorithm: src_filter_to_algorithm(opts.src_filter),
        fixed_rate,
        prefer_family: ClockFamily::Auto,
        fallback_rate: FallbackRatePolicy::Nearest,
        ring_buffer_ms: clamp_ring_buffer_ms(u32::from(opts.buffer.get())),
        bit_perfect: false,
        dither: dither_to_legacy(opts.dither),
    };
    (legacy, device)
}

fn optimal_legacy_audio(opts: &OptimalOpts) -> (LegacyAudio, Option<String>) {
    let legacy = LegacyAudio {
        exclusive_mode: ExclusiveMode::Strict,
        fallback_policy: FallbackPolicy::Nearest,
        dsd_mode: DsdMode::Pcm,
        resampler_mode: ResamplerMode::Auto,
        resampler_algorithm: src_filter_to_algorithm(opts.src_filter),
        fixed_rate: 0,
        prefer_family: ClockFamily::Auto,
        fallback_rate: rate_fallback_to_legacy(opts.rate_fallback),
        ring_buffer_ms: clamp_ring_buffer_ms(u32::from(opts.buffer.get())),
        bit_perfect: false,
        dither: dither_to_legacy(opts.dither),
    };
    (legacy, opts.device.clone())
}

fn strict_legacy_audio(opts: &StrictOpts) -> (LegacyAudio, Option<String>) {
    let legacy = LegacyAudio {
        exclusive_mode: ExclusiveMode::Strict,
        fallback_policy: FallbackPolicy::Fail,
        dsd_mode: DsdMode::Pcm,
        resampler_mode: ResamplerMode::Native,
        resampler_algorithm: ResamplerAlgorithm::default(),
        fixed_rate: 0,
        prefer_family: ClockFamily::Auto,
        fallback_rate: FallbackRatePolicy::default(),
        ring_buffer_ms: clamp_ring_buffer_ms(u32::from(opts.buffer.get())),
        bit_perfect: true,
        dither: ResamplerDither::Off,
    };
    (legacy, opts.device.clone())
}

/// Запасная частота для случая, когда `StreamDesc::rate` не заполнена
/// (нулевая/`StreamDesc::default()`, ТЗ-57, §2.3): `SampleRate` не может
/// быть нулевым, поэтому мост не теряет инвариант нового пути. Константа
/// заведомо ненулевая — вычисляется на этапе компиляции.
const FALLBACK_RATE: SampleRate = match SampleRate::new(44_100) {
    Some(rate) => rate,
    None => panic!("FALLBACK_RATE_HZ must be non-zero"),
};

/// `OutputApi` Shared-вывода на этой платформе (§2.3, ТЗ-109, ТЗ-110): тот
/// же выбор, что у `BackendCaps::current()` (`src/audio/backend/mod.rs`) —
/// `cfg!` без I/O, статично для сборки.
fn shared_output_api() -> OutputApi {
    if cfg!(target_os = "windows") {
        OutputApi::WasapiShared
    } else if cfg!(target_os = "macos") {
        OutputApi::CoreAudio
    } else {
        OutputApi::PipeWire
    }
}

/// Короткое имя формата из [`StreamDesc::format`] (`format_name`,
/// `src/audio/player.rs`) → [`OutputSampleFormat`] нового пути (§2.3).
/// `"I24"` — 4-байтовый контейнер (ОВ-32), как `OutputSampleFormat::S24Le`,
/// а не 3-байтовый `S24_3Le`, которого старый путь не производит.
/// Нераспознанное имя (`"?"`) — `F32`, формат shared-микшера (§2.3).
fn output_sample_format_from_desc(format: &str) -> OutputSampleFormat {
    match format {
        "I16" => OutputSampleFormat::S16Le,
        "I24" => OutputSampleFormat::S24Le,
        "I32" => OutputSampleFormat::S32Le,
        _ => OutputSampleFormat::F32,
    }
}

/// Безопасное число каналов потока (без `as`, §2.3): `StreamDesc::channels`
/// (`u16`) в диапазон `u8`/`NonZeroU8`, с насыщением сверху и минимумом 1
/// канал, если поле не заполнено (`StreamDesc::default()`).
fn channel_count_u8(channels: u16) -> u8 {
    u8::try_from(channels).unwrap_or(u8::MAX)
}

/// `DeviceCaps` открытого Shared-потока старого пути (ОВС-12, §2.3, §6.3,
/// ADR-20, С5-мост): не возможности устройства, а точный снимок того, что
/// уже открыто `StreamDesc`, чтобы `plan()` строил `PathPlan` для
/// Совместимого режима, пока исполнителем остаётся `Player`. Мост убирается
/// вместе со `StreamDesc` на этапе С6.
pub(crate) fn shared_caps_from_desc(desc: &StreamDesc) -> DeviceCaps {
    let rate = SampleRate::new(desc.rate).unwrap_or(FALLBACK_RATE);
    DeviceCaps {
        api: shared_output_api(),
        access: AccessKind::Shared,
        name: desc.device.clone(),
        rates: RateSet::Discrete(SmallVec::from_slice(&[rate])),
        formats: SmallVec::from_slice(&[output_sample_format_from_desc(desc.format)]),
        channels: SmallVec::from_slice(&[channel_count_u8(desc.channels)]),
    }
}

/// Формат потока старого пути (§2.3, ADR-05, С5-мост) — те же безопасные
/// конвертеры; используется и для `OpenedFacts`, и для события `Opened`
/// движка, пока путь строится не во всех режимах; убирается вместе со
/// `StreamDesc` на этапе С6.
pub(crate) fn stream_format_from_desc(desc: &StreamDesc) -> StreamFormat {
    StreamFormat {
        rate: SampleRate::new(desc.rate).unwrap_or(FALLBACK_RATE),
        sample: output_sample_format_from_desc(desc.format),
        channels: NonZeroU8::new(channel_count_u8(desc.channels)).unwrap_or(NonZeroU8::MIN),
    }
}

/// `OpenedFacts` открытого Shared-потока старого пути (ОВС-12, §2.3, §6.3,
/// ADR-20, С5-мост): старый cpal shared-путь не читает hw_params устройства,
/// поэтому `driver` — `DriverFormat::Unreadable` с запрошенным форматом, а
/// не `Confirmed` (ОВ-14). Резервирование не применяется к Shared
/// (`ReservationView::NotApplicable`), буфер устройства неизвестен (`None`).
/// `os` передаётся вызывающим — он получен отдельной пробой `OsLevelProbe`
/// (§6.26), не из `StreamDesc`. Мост убирается вместе со `StreamDesc` на
/// этапе С6.
pub(crate) fn opened_facts_from_desc(desc: &StreamDesc, os: OsStage) -> OpenedFacts {
    let requested = stream_format_from_desc(desc);
    OpenedFacts {
        access: AccessKind::Shared,
        api: shared_output_api(),
        device_name: desc.device.clone(),
        driver: DriverFormat::Unreadable { requested },
        reservation: ReservationView::NotApplicable,
        os,
        device_buffer: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::backend::SharedDeviceId;
    use crate::audio::format::SampleRate;

    #[test]
    fn compatible_maps_shared_and_fixed_rate() {
        let mut settings = ModeSettings::default();
        settings.compatible.fixed_rate = SampleRate::new(96_000);
        settings.compatible.device = SharedDeviceChoice::Named(SharedDeviceId::new("hw:CARD=X"));
        settings.compatible.dither = Dither::Off;
        let (legacy, device) = legacy_audio(&settings, ModeKind::Compatible);
        assert_eq!(legacy.exclusive_mode, ExclusiveMode::Off);
        assert_eq!(legacy.dsd_mode, DsdMode::Pcm);
        assert_eq!(legacy.resampler_mode, ResamplerMode::Fixed);
        assert_eq!(legacy.fixed_rate, 96_000);
        assert_eq!(legacy.dither, ResamplerDither::Off);
        assert_eq!(device, Some(String::from("hw:CARD=X")));
    }

    #[test]
    fn optimal_maps_exclusive_strict_and_rate_fallback() {
        let mut settings = ModeSettings::default();
        settings.optimal.rate_fallback = RateFallbackRule::NoDownsample;
        settings.optimal.device = Some(String::from("hw:CARD=Y"));
        let (legacy, device) = legacy_audio(&settings, ModeKind::Optimal);
        assert_eq!(legacy.exclusive_mode, ExclusiveMode::Strict);
        assert_eq!(legacy.resampler_mode, ResamplerMode::Auto);
        assert_eq!(legacy.fallback_policy, FallbackPolicy::Nearest);
        assert_eq!(legacy.fallback_rate, FallbackRatePolicy::NeverDownsample);
        assert_eq!(device, Some(String::from("hw:CARD=Y")));
    }

    #[test]
    fn strict_maps_native_no_src_bit_perfect() {
        let settings = ModeSettings::default();
        let (legacy, _device) = legacy_audio(&settings, ModeKind::Strict);
        assert_eq!(legacy.exclusive_mode, ExclusiveMode::Strict);
        assert_eq!(legacy.resampler_mode, ResamplerMode::Native);
        assert_eq!(legacy.fallback_policy, FallbackPolicy::Fail);
        assert_eq!(legacy.dither, ResamplerDither::Off);
        assert!(legacy.bit_perfect);
    }

    #[test]
    fn optimal_never_falls_back_to_shared() {
        let settings = ModeSettings::default();
        let (legacy, _device) = legacy_audio(&settings, ModeKind::Optimal);
        // `ExclusiveMode::Auto` откатывается в shared при неудаче —
        // решение 4 (ОВС-12) запрещает это для Оптимального режима.
        assert_ne!(legacy.exclusive_mode, ExclusiveMode::Auto);
        assert_eq!(legacy.exclusive_mode, ExclusiveMode::Strict);
    }

    use crate::audio::format::{BitDepth, ChannelLayout, Codec, Container, SourceFormat, SourceKind};
    use crate::audio::path::plan;
    use crate::settings::playback::PathPolicy;

    fn desc_48k_f32_stereo() -> StreamDesc {
        StreamDesc {
            device: "pipewire".to_owned(),
            rate: 48_000,
            channels: 2,
            format: "F32",
            ..StreamDesc::default()
        }
    }

    #[test]
    fn shared_caps_describe_opened_stream() {
        let desc = desc_48k_f32_stereo();
        let caps = shared_caps_from_desc(&desc);
        assert_eq!(caps.access, AccessKind::Shared);
        assert_eq!(caps.name, "pipewire");
        assert_eq!(caps.rates, RateSet::Discrete(SmallVec::from_slice(&[SampleRate::new(48_000).unwrap()])));
        assert_eq!(caps.formats.as_slice(), &[OutputSampleFormat::F32]);
        assert_eq!(caps.channels.as_slice(), &[2]);
    }

    #[test]
    fn shared_caps_unknown_format_is_f32() {
        let mut desc = desc_48k_f32_stereo();
        desc.format = "?";
        let caps = shared_caps_from_desc(&desc);
        assert_eq!(caps.formats.as_slice(), &[OutputSampleFormat::F32]);
    }

    #[test]
    fn stream_format_from_desc_maps_rate_format_channels() {
        let mut desc = desc_48k_f32_stereo();
        desc.rate = 96_000;
        desc.format = "I24";
        desc.channels = 1;
        let format = stream_format_from_desc(&desc);
        assert_eq!(format.rate, SampleRate::new(96_000).unwrap());
        assert_eq!(format.sample, OutputSampleFormat::S24Le);
        assert_eq!(format.channels, NonZeroU8::new(1).unwrap());

        let default_format = stream_format_from_desc(&StreamDesc::default());
        assert_eq!(default_format.rate, FALLBACK_RATE);
        assert_eq!(default_format.sample, OutputSampleFormat::F32);
        assert_eq!(default_format.channels, NonZeroU8::new(1).unwrap());
    }

    #[test]
    fn opened_facts_shared_no_reservation_no_buffer() {
        let desc = desc_48k_f32_stereo();
        let facts = opened_facts_from_desc(&desc, OsStage::Unknown);
        assert_eq!(facts.access, AccessKind::Shared);
        assert_eq!(facts.reservation, ReservationView::NotApplicable);
        assert_eq!(facts.device_buffer, None);
        assert!(matches!(facts.driver, crate::audio::backend::DriverFormat::Unreadable { .. }));
    }

    #[test]
    fn shared_caps_build_a_plan_with_resample() {
        let src = SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: SampleRate::new(44_100).unwrap(),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: BitDepth::new(16).unwrap() },
        };
        let desc = desc_48k_f32_stereo();
        let caps = shared_caps_from_desc(&desc);
        let opts = CompatibleOpts::default();
        let plan_result = plan(&src, &caps, PathPolicy::Compatible(&opts));
        assert!(plan_result.is_ok(), "{:?}", plan_result);
    }
}
