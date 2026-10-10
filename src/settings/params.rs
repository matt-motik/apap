//! Таблица матрицы режимов §5 ТЗ: `ParamId`, `Availability`, `PARAMS`,
//! `availability` (ТЗ-95, ОВ-28, ОВС-17, §2.2).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use super::ModeKind;

/// Идентификатор параметра тракта — строка матрицы §5 (§2.2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ParamId {
    Access,
    Device,
    Lock,
    OutputRate,
    FixedRate,
    RateFallback,
    Src,
    SrcFilter,
    SampleFormat,
    ZeroPad,
    Truncation,
    DitherApplied,
    DitherKind,
    Channels,
    Downmix,
    MonoToStereo,
    ChannelPad,
    FloatSource,
    Volume,
    Mute,
    VolumeLock,
    Equalizer,
    DsdNative,
    DsdDop,
    DsdToPcm,
    DsdAboveDac,
    DsdPcmParams,
    DsdFilter,
    DsdGainComp,
    OnIncompatible,
    OnBadFile,
    OnDeviceLost,
    Buffer,
    DeviceBuffer,
    RtPriority,
    Badge,
    Test,
    Md5,
    UnderrunXrun,
}

/// Показанное значение зафиксированного режимом параметра («Ф» в матрице §5):
/// ключ подписи, которую рисует диалог настроек (ТЗ-95, §2.2).
pub type FixedValue = &'static str;

/// Причина недоступности параметра («—» в матрице §5): ключ пояснения,
/// которое рисует диалог настроек (ТЗ-95, §2.2).
pub type UnavailableWhy = &'static str;

/// Доступность параметра в режиме (ТЗ-95, ОВ-28, §2.2).
#[derive(Clone, PartialEq, Debug)]
pub enum Availability {
    /// «Н» — пользователь меняет значение.
    Configurable,
    /// «Ф» — видно, неактивно, с текущим значением и пояснением «задано режимом».
    FixedByMode { shown: FixedValue },
    /// «—» — видно неактивным с причиной (например, «отключено режимом»).
    Unavailable { why: UnavailableWhy },
    /// Виден, неактивен, «пока не реализовано» (Native DSD, ремодуляция, эквалайзер).
    NotImplemented,
}

/// Способ применения изменения параметра активного режима (ОВС-17, §6.18).
/// Изменение параметра неактивного режима — всегда только память.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApplyKind {
    /// Параметр не влияет на открытый поток (например, «Тест», MD5, реакции).
    Memory,
    /// Параметр звука: одно переоткрытие текущего трека с позиции, состояние
    /// воспроизведения сохраняется; устройство не освобождается, в Exclusive
    /// при необходимости `configure`.
    ReopenAtPosition,
    /// Устройство: освобождение, захват нового, продолжение с той же позиции.
    SwitchDevice,
}

/// Вкладка и элемент диалога настроек для кнопки «перейти к настройке»
/// (ТЗ-81, §2.2). Разметка диалога не определена этим шагом — заполняется
/// отдельным шагом, когда появится вкладка.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SettingsAnchor {
    pub tab: &'static str,
    pub element: &'static str,
}

/// Строка таблицы матрицы §5: параметр, подпись, доступность по режиму,
/// способ применения изменения, якорь диалога настроек (ТЗ-95, ОВС-17, §2.2).
pub struct ParamDescriptor {
    pub id: ParamId,
    pub label_key: &'static str,
    pub availability: fn(ModeKind) -> Availability,
    pub apply: ApplyKind,
    pub settings_anchor: Option<SettingsAnchor>,
}

fn availability_access(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::FixedByMode { shown: "shared" },
        ModeKind::Optimal | ModeKind::Strict => Availability::FixedByMode { shown: "exclusive" },
    }
}

fn availability_device(_mode: ModeKind) -> Availability {
    Availability::Configurable
}

fn availability_lock(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "hidden_with_hint" },
        ModeKind::Optimal | ModeKind::Strict => Availability::Configurable,
    }
}

fn availability_output_rate(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "source_rate" }
}

fn availability_fixed_rate(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Configurable,
        ModeKind::Optimal | ModeKind::Strict => Availability::Unavailable { why: "fixed_rate_unavailable" },
    }
}

fn availability_rate_fallback(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::FixedByMode { shown: "server_accepted_rate" },
        ModeKind::Optimal => Availability::Configurable,
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_src(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::FixedByMode { shown: "src_on_server_mismatch" },
        ModeKind::Optimal => Availability::FixedByMode { shown: "src_on_device_mismatch" },
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_src_filter(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::Configurable,
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_sample_format(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "sample_format_by_mode" }
}

fn availability_zero_pad(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "float_no_padding" },
        ModeKind::Optimal | ModeKind::Strict => Availability::FixedByMode { shown: "allowed" },
    }
}

fn availability_truncation(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "not_needed_float" },
        ModeKind::Optimal => Availability::FixedByMode { shown: "only_if_device_too_narrow" },
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_dither_applied(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::FixedByMode { shown: "after_lossy_requantize" },
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_dither_kind(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::Configurable,
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_channels(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "source_channels" }
}

fn availability_downmix(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::FixedByMode { shown: "itu_bs775_no_lfe" },
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_mono_to_stereo(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "duplicate_lossless" }
}

fn availability_channel_pad(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "front_channels_then_silence" }
}

fn availability_float_source(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::FixedByMode { shown: "float32_lossless_float64_lossy" },
        ModeKind::Optimal => Availability::FixedByMode { shown: "quantize_tpdf_not_bit_perfect" },
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_volume(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::Configurable,
        ModeKind::Strict => Availability::Unavailable { why: "no_volume" },
    }
}

fn availability_mute(_mode: ModeKind) -> Availability {
    Availability::Configurable
}

fn availability_volume_lock(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "no_checkbox" },
        ModeKind::Optimal => Availability::Configurable,
        ModeKind::Strict => Availability::Unavailable { why: "no_volume" },
    }
}

fn availability_equalizer(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::NotImplemented,
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_dsd_native(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "disabled_by_mode" },
        ModeKind::Optimal | ModeKind::Strict => Availability::NotImplemented,
    }
}

fn availability_dsd_dop(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "disabled_by_mode" },
        ModeKind::Optimal => Availability::FixedByMode { shown: "first_available_path" },
        ModeKind::Strict => Availability::FixedByMode { shown: "only_path" },
    }
}

fn availability_dsd_to_pcm(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::FixedByMode { shown: "only_path" },
        ModeKind::Optimal => Availability::FixedByMode { shown: "second_path_after_dop" },
        ModeKind::Strict => Availability::Unavailable { why: "disabled_by_mode" },
    }
}

fn availability_dsd_above_dac(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "always_pcm" },
        ModeKind::Optimal => Availability::Configurable,
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_dsd_pcm_params(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::FixedByMode { shown: "dsd_to_pcm_rate_bits" },
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_dsd_filter(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::Configurable,
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_dsd_gain_comp(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::Configurable,
        ModeKind::Strict => Availability::Unavailable { why: "incompatible" },
    }
}

fn availability_on_incompatible(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible | ModeKind::Optimal => Availability::Unavailable { why: "no_incompatibility" },
        ModeKind::Strict => Availability::FixedByMode { shown: "skip_with_message" },
    }
}

fn availability_on_bad_file(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "skip_with_message" }
}

fn availability_on_device_lost(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::FixedByMode { shown: "follow_system_or_stop_with_error" },
        ModeKind::Optimal | ModeKind::Strict => Availability::FixedByMode { shown: "stop_with_error_auto_recapture" },
    }
}

fn availability_buffer(_mode: ModeKind) -> Availability {
    Availability::Configurable
}

fn availability_device_buffer(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "device_sets_it" },
        ModeKind::Optimal | ModeKind::Strict => Availability::Configurable,
    }
}

fn availability_rt_priority(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::FixedByMode { shown: "unchanged" },
        ModeKind::Optimal | ModeKind::Strict => Availability::FixedByMode { shown: "requested_via_rtkit" },
    }
}

fn availability_badge(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "badge_states_by_mode" }
}

fn availability_test(mode: ModeKind) -> Availability {
    match mode {
        ModeKind::Compatible => Availability::Unavailable { why: "hidden_with_hint" },
        ModeKind::Optimal | ModeKind::Strict => Availability::Configurable,
    }
}

fn availability_md5(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "checked" }
}

fn availability_underrun_xrun(_mode: ModeKind) -> Availability {
    Availability::FixedByMode { shown: "tracked_by_mode" }
}

/// Единая таблица матрицы §5 ТЗ (ТЗ-95, ОВ-28, ОВС-17, §2.2, §6.18): источник
/// доступности для диалога настроек (ТЗ-95) и фильтра рекомендаций (ТЗ-82).
pub static PARAMS: &[ParamDescriptor] = &[
    ParamDescriptor { id: ParamId::Access, label_key: "access", availability: availability_access, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Device, label_key: "device", availability: availability_device, apply: ApplyKind::SwitchDevice, settings_anchor: None },
    ParamDescriptor { id: ParamId::Lock, label_key: "lock", availability: availability_lock, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::OutputRate, label_key: "output_rate", availability: availability_output_rate, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::FixedRate, label_key: "fixed_rate", availability: availability_fixed_rate, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::RateFallback, label_key: "rate_fallback", availability: availability_rate_fallback, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::Src, label_key: "src", availability: availability_src, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::SrcFilter, label_key: "src_filter", availability: availability_src_filter, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::SampleFormat, label_key: "sample_format", availability: availability_sample_format, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::ZeroPad, label_key: "zero_pad", availability: availability_zero_pad, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Truncation, label_key: "truncation", availability: availability_truncation, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::DitherApplied, label_key: "dither_applied", availability: availability_dither_applied, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::DitherKind, label_key: "dither_kind", availability: availability_dither_kind, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::Channels, label_key: "channels", availability: availability_channels, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Downmix, label_key: "downmix", availability: availability_downmix, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::MonoToStereo, label_key: "mono_to_stereo", availability: availability_mono_to_stereo, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::ChannelPad, label_key: "channel_pad", availability: availability_channel_pad, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::FloatSource, label_key: "float_source", availability: availability_float_source, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Volume, label_key: "volume", availability: availability_volume, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Mute, label_key: "mute", availability: availability_mute, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::VolumeLock, label_key: "volume_lock", availability: availability_volume_lock, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::Equalizer, label_key: "equalizer", availability: availability_equalizer, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::DsdNative, label_key: "dsd_native", availability: availability_dsd_native, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::DsdDop, label_key: "dsd_dop", availability: availability_dsd_dop, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::DsdToPcm, label_key: "dsd_to_pcm", availability: availability_dsd_to_pcm, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::DsdAboveDac, label_key: "dsd_above_dac", availability: availability_dsd_above_dac, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::DsdPcmParams, label_key: "dsd_pcm_params", availability: availability_dsd_pcm_params, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::DsdFilter, label_key: "dsd_filter", availability: availability_dsd_filter, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::DsdGainComp, label_key: "dsd_gain_comp", availability: availability_dsd_gain_comp, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::OnIncompatible, label_key: "on_incompatible", availability: availability_on_incompatible, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::OnBadFile, label_key: "on_bad_file", availability: availability_on_bad_file, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::OnDeviceLost, label_key: "on_device_lost", availability: availability_on_device_lost, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Buffer, label_key: "buffer", availability: availability_buffer, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::DeviceBuffer, label_key: "device_buffer", availability: availability_device_buffer, apply: ApplyKind::ReopenAtPosition, settings_anchor: None },
    ParamDescriptor { id: ParamId::RtPriority, label_key: "rt_priority", availability: availability_rt_priority, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Badge, label_key: "badge", availability: availability_badge, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Test, label_key: "test", availability: availability_test, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::Md5, label_key: "md5", availability: availability_md5, apply: ApplyKind::Memory, settings_anchor: None },
    ParamDescriptor { id: ParamId::UnderrunXrun, label_key: "underrun_xrun", availability: availability_underrun_xrun, apply: ApplyKind::Memory, settings_anchor: None },
];

/// Доступность параметра в режиме: поиск в `PARAMS` (ТЗ-95, ОВ-28, §2.2).
pub fn availability(id: ParamId, mode: ModeKind) -> Availability {
    for descriptor in PARAMS {
        if descriptor.id == id {
            return (descriptor.availability)(mode);
        }
    }
    Availability::Unavailable { why: "param_missing" }
}
