//! Адаптер новой модели режимов (ADR-04, §2.2) к старому аудио-пути
//! `LegacyAudio` (ОВС-12, §8 С4). Старый путь остаётся единственным
//! исполнителем вывода на Linux до замены в С6; этот модуль переводит
//! `ModeSettings` активного режима в параметры, которые старый путь уже
//! умеет применять (`EngineCmd::SetLegacyAudio` / `EngineCmd::SetDevice`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

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
}
