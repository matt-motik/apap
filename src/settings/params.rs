//! Таблица матрицы режимов §5 ТЗ: `ParamId`, `Availability`, `PARAMS`,
//! `availability` (ТЗ-95, ОВ-28, ОВС-17, §2.2).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use super::playback::{CompatibleOpts, ModeSettings, OptimalOpts, StrictOpts};
use super::ModeKind;
use smallvec::SmallVec;

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
/// Изменение параметра неактивного режима — всегда только память. Порядок
/// вариантов — порядок силы эффекта (И-Р26): `Ord` даёт наибольший через `max`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
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

/// Способ применения параметра: поиск в `PARAMS` (ОВС-14, ОВС-17, §6.18,
/// ADR-22). Движок выбирает действие по этой функции и по тому, активен ли
/// режим изменённого параметра.
pub fn apply_kind(id: ParamId) -> ApplyKind {
    for descriptor in PARAMS {
        if descriptor.id == id {
            return descriptor.apply;
        }
    }
    ApplyKind::Memory
}

/// Изменённые параметры одного «Сохранить» диалога (ОВС-14, §2.2). Пустое
/// множество не отправляется.
#[derive(Clone, PartialEq, Debug)]
pub struct ModeSettingsDiff {
    pub changed: SmallVec<[(ModeKind, ParamId); 8]>,
}

/// Команда применения опций режимов: новые значения и что изменилось
/// (ОВС-14, §2.2).
#[derive(Clone, PartialEq, Debug)]
pub struct ModeSettingsUpdate {
    pub settings: ModeSettings,
    /// `None` — начальная загрузка при старте: применить всё без переоткрытия
    /// (потока ещё нет).
    pub diff: Option<ModeSettingsDiff>,
}

/// Поля `CompatibleOpts` → `ParamId`, без `..` — новое поле без строки здесь
/// не компилируется (§2.2).
fn diff_compatible(a: &CompatibleOpts, b: &CompatibleOpts, out: &mut SmallVec<[(ModeKind, ParamId); 8]>) {
    let CompatibleOpts { device: a_device, fixed_rate: a_fixed_rate, src_filter: a_src_filter, dither: a_dither, dsd_filter: a_dsd_filter, dsd_gain_comp: a_dsd_gain_comp, buffer: a_buffer } = a;
    let CompatibleOpts { device: b_device, fixed_rate: b_fixed_rate, src_filter: b_src_filter, dither: b_dither, dsd_filter: b_dsd_filter, dsd_gain_comp: b_dsd_gain_comp, buffer: b_buffer } = b;
    if a_device != b_device {
        out.push((ModeKind::Compatible, ParamId::Device));
    }
    if a_fixed_rate != b_fixed_rate {
        out.push((ModeKind::Compatible, ParamId::FixedRate));
    }
    if a_src_filter != b_src_filter {
        out.push((ModeKind::Compatible, ParamId::SrcFilter));
    }
    if a_dither != b_dither {
        out.push((ModeKind::Compatible, ParamId::DitherKind));
    }
    if a_dsd_filter != b_dsd_filter {
        out.push((ModeKind::Compatible, ParamId::DsdFilter));
    }
    if a_dsd_gain_comp != b_dsd_gain_comp {
        out.push((ModeKind::Compatible, ParamId::DsdGainComp));
    }
    if a_buffer != b_buffer {
        out.push((ModeKind::Compatible, ParamId::Buffer));
    }
}

/// Поля `OptimalOpts` → `ParamId`, без `..` (§2.2).
fn diff_optimal(a: &OptimalOpts, b: &OptimalOpts, out: &mut SmallVec<[(ModeKind, ParamId); 8]>) {
    let OptimalOpts {
        device: a_device,
        rate_fallback: a_rate_fallback,
        src_filter: a_src_filter,
        dither: a_dither,
        volume_lock: a_volume_lock,
        dsd_above_dac: a_dsd_above_dac,
        dsd_filter: a_dsd_filter,
        dsd_gain_comp: a_dsd_gain_comp,
        buffer: a_buffer,
        device_buffer: a_device_buffer,
    } = a;
    let OptimalOpts {
        device: b_device,
        rate_fallback: b_rate_fallback,
        src_filter: b_src_filter,
        dither: b_dither,
        volume_lock: b_volume_lock,
        dsd_above_dac: b_dsd_above_dac,
        dsd_filter: b_dsd_filter,
        dsd_gain_comp: b_dsd_gain_comp,
        buffer: b_buffer,
        device_buffer: b_device_buffer,
    } = b;
    if a_device != b_device {
        out.push((ModeKind::Optimal, ParamId::Device));
    }
    if a_rate_fallback != b_rate_fallback {
        out.push((ModeKind::Optimal, ParamId::RateFallback));
    }
    if a_src_filter != b_src_filter {
        out.push((ModeKind::Optimal, ParamId::SrcFilter));
    }
    if a_dither != b_dither {
        out.push((ModeKind::Optimal, ParamId::DitherKind));
    }
    if a_volume_lock != b_volume_lock {
        out.push((ModeKind::Optimal, ParamId::VolumeLock));
    }
    if a_dsd_above_dac != b_dsd_above_dac {
        out.push((ModeKind::Optimal, ParamId::DsdAboveDac));
    }
    if a_dsd_filter != b_dsd_filter {
        out.push((ModeKind::Optimal, ParamId::DsdFilter));
    }
    if a_dsd_gain_comp != b_dsd_gain_comp {
        out.push((ModeKind::Optimal, ParamId::DsdGainComp));
    }
    if a_buffer != b_buffer {
        out.push((ModeKind::Optimal, ParamId::Buffer));
    }
    if a_device_buffer != b_device_buffer {
        out.push((ModeKind::Optimal, ParamId::DeviceBuffer));
    }
}

/// Поля `StrictOpts` → `ParamId`, без `..` (§2.2).
fn diff_strict(a: &StrictOpts, b: &StrictOpts, out: &mut SmallVec<[(ModeKind, ParamId); 8]>) {
    let StrictOpts { device: a_device, buffer: a_buffer, device_buffer: a_device_buffer } = a;
    let StrictOpts { device: b_device, buffer: b_buffer, device_buffer: b_device_buffer } = b;
    if a_device != b_device {
        out.push((ModeKind::Strict, ParamId::Device));
    }
    if a_buffer != b_buffer {
        out.push((ModeKind::Strict, ParamId::Buffer));
    }
    if a_device_buffer != b_device_buffer {
        out.push((ModeKind::Strict, ParamId::DeviceBuffer));
    }
}

impl ModeSettings {
    /// Разница опций трёх режимов — вход `SetModeSettings(ModeSettingsUpdate)`
    /// по «Сохранить» диалога настроек (ОВС-14, §2.2).
    pub fn diff(&self, new: &ModeSettings) -> ModeSettingsDiff {
        let mut changed = SmallVec::new();
        diff_compatible(&self.compatible, &new.compatible, &mut changed);
        diff_optimal(&self.optimal, &new.optimal, &mut changed);
        diff_strict(&self.strict, &new.strict, &mut changed);
        ModeSettingsDiff { changed }
    }
}

impl ModeSettingsDiff {
    /// Способ применения с наибольшим эффектом среди изменений активного
    /// режима; изменения неактивных режимов не считаются (И-Р26, §6.18):
    /// `SwitchDevice` > `ReopenAtPosition` > `Memory`.
    pub fn strongest(&self, active: ModeKind) -> Option<ApplyKind> {
        self.changed
            .iter()
            .filter(|(mode, _)| *mode == active)
            .map(|(_, id)| apply_kind(*id))
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Вид доступности без учёта содержимого (ключа подписи/причины) — для
    /// сверки с матрицей §5 ТЗ по категории «Ф/Н/—/не реализовано».
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Kind {
        Configurable,
        FixedByMode,
        Unavailable,
        NotImplemented,
    }

    fn kind(a: &Availability) -> Kind {
        match a {
            Availability::Configurable => Kind::Configurable,
            Availability::FixedByMode { .. } => Kind::FixedByMode,
            Availability::Unavailable { .. } => Kind::Unavailable,
            Availability::NotImplemented => Kind::NotImplemented,
        }
    }

    /// Ожидаемый вид доступности (Совместимый, Оптимальный, Строгий) —
    /// построено независимо по матрице §5 `docs/01_audio_modes_v1.0/02_tz.md`
    /// (строки «Реакция на отказ захвата» и «Платформы» матрицы не являются
    /// параметрами тракта и в `ParamId` не представлены). Принятые трактовки:
    /// громкость в Оптимальном — «Н» (подслучай фиксации громкости учитывается
    /// отдельно); эквалайзер — «пока не реализовано» в Совместимом/Оптимальном,
    /// «—» в Строгом.
    const EXPECTED: &[(ParamId, Kind, Kind, Kind)] = &[
        (ParamId::Access, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::Device, Kind::Configurable, Kind::Configurable, Kind::Configurable),
        (ParamId::Lock, Kind::Unavailable, Kind::Configurable, Kind::Configurable),
        (ParamId::OutputRate, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::FixedRate, Kind::Configurable, Kind::Unavailable, Kind::Unavailable),
        (ParamId::RateFallback, Kind::FixedByMode, Kind::Configurable, Kind::Unavailable),
        (ParamId::Src, Kind::FixedByMode, Kind::FixedByMode, Kind::Unavailable),
        (ParamId::SrcFilter, Kind::Configurable, Kind::Configurable, Kind::Unavailable),
        (ParamId::SampleFormat, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::ZeroPad, Kind::Unavailable, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::Truncation, Kind::Unavailable, Kind::FixedByMode, Kind::Unavailable),
        (ParamId::DitherApplied, Kind::FixedByMode, Kind::FixedByMode, Kind::Unavailable),
        (ParamId::DitherKind, Kind::Configurable, Kind::Configurable, Kind::Unavailable),
        (ParamId::Channels, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::Downmix, Kind::FixedByMode, Kind::FixedByMode, Kind::Unavailable),
        (ParamId::MonoToStereo, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::ChannelPad, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::FloatSource, Kind::FixedByMode, Kind::FixedByMode, Kind::Unavailable),
        (ParamId::Volume, Kind::Configurable, Kind::Configurable, Kind::Unavailable),
        (ParamId::Mute, Kind::Configurable, Kind::Configurable, Kind::Configurable),
        (ParamId::VolumeLock, Kind::Unavailable, Kind::Configurable, Kind::Unavailable),
        (ParamId::Equalizer, Kind::NotImplemented, Kind::NotImplemented, Kind::Unavailable),
        (ParamId::DsdNative, Kind::Unavailable, Kind::NotImplemented, Kind::NotImplemented),
        (ParamId::DsdDop, Kind::Unavailable, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::DsdToPcm, Kind::FixedByMode, Kind::FixedByMode, Kind::Unavailable),
        (ParamId::DsdAboveDac, Kind::Unavailable, Kind::Configurable, Kind::Unavailable),
        (ParamId::DsdPcmParams, Kind::FixedByMode, Kind::FixedByMode, Kind::Unavailable),
        (ParamId::DsdFilter, Kind::Configurable, Kind::Configurable, Kind::Unavailable),
        (ParamId::DsdGainComp, Kind::Configurable, Kind::Configurable, Kind::Unavailable),
        (ParamId::OnIncompatible, Kind::Unavailable, Kind::Unavailable, Kind::FixedByMode),
        (ParamId::OnBadFile, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::OnDeviceLost, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::Buffer, Kind::Configurable, Kind::Configurable, Kind::Configurable),
        (ParamId::DeviceBuffer, Kind::Unavailable, Kind::Configurable, Kind::Configurable),
        (ParamId::RtPriority, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::Badge, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::Test, Kind::Unavailable, Kind::Configurable, Kind::Configurable),
        (ParamId::Md5, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
        (ParamId::UnderrunXrun, Kind::FixedByMode, Kind::FixedByMode, Kind::FixedByMode),
    ];

    #[test]
    fn params_table_matches_matrix() {
        assert_eq!(PARAMS.len(), 39, "PARAMS должен содержать ровно 39 параметров матрицы §5");
        assert_eq!(EXPECTED.len(), 39, "ожидаемая таблица должна покрывать все 39 параметров");

        let mut seen = HashSet::new();
        for descriptor in PARAMS {
            assert!(
                seen.insert(descriptor.id),
                "ParamId {:?} встречается в PARAMS более одного раза",
                descriptor.id
            );
        }
        assert_eq!(seen.len(), 39, "каждый ParamId должен встречаться в PARAMS ровно один раз");

        for (id, exp_compat, exp_optimal, exp_strict) in EXPECTED {
            let Some(descriptor) = PARAMS.iter().find(|d| d.id == *id) else {
                panic!("параметр {id:?} из матрицы §5 отсутствует в PARAMS");
            };
            let actual_compat = kind(&(descriptor.availability)(ModeKind::Compatible));
            let actual_optimal = kind(&(descriptor.availability)(ModeKind::Optimal));
            let actual_strict = kind(&(descriptor.availability)(ModeKind::Strict));
            assert_eq!(actual_compat, *exp_compat, "{id:?}: Совместимый режим");
            assert_eq!(actual_optimal, *exp_optimal, "{id:?}: Оптимальный режим");
            assert_eq!(actual_strict, *exp_strict, "{id:?}: Строгий режим");
        }
    }

    /// §7.2 `every_audio_setting_changes_observable` (ТЗ-96): модельная часть —
    /// изменение одного поля опций режима даёт в `ModeSettings::diff` ровно
    /// один `ParamId` того же режима. `optimal.dsd_above_dac` не входит: тип
    /// `DsdAboveDac` сейчас имеет единственный вариант (решение 6, §2.2), второе
    /// значение для сравнения не существует.
    #[test]
    fn every_audio_setting_changes_observable() {
        use crate::audio::backend::SharedDeviceId;
        use crate::audio::format::SampleRate;
        use crate::settings::playback::{
            BufferMs, DeviceBuffer, Dither, DsdFilter, RateFallbackRule, SharedDeviceChoice, SrcFilter,
        };

        let base = ModeSettings::default();
        let Some(rate) = SampleRate::new(48_000) else {
            panic!("48000 Гц — допустимая частота");
        };
        let Some(buffer) = BufferMs::new(2_000) else {
            panic!("2000 мс — допустимый буфер плеера");
        };

        let mut compatible_device = base.clone();
        compatible_device.compatible.device = SharedDeviceChoice::Named(SharedDeviceId::new("dac"));
        let mut compatible_fixed_rate = base.clone();
        compatible_fixed_rate.compatible.fixed_rate = Some(rate);
        let mut compatible_src_filter = base.clone();
        compatible_src_filter.compatible.src_filter = SrcFilter::Slow;
        let mut compatible_dither = base.clone();
        compatible_dither.compatible.dither = Dither::Off;
        let mut compatible_dsd_filter = base.clone();
        compatible_dsd_filter.compatible.dsd_filter = DsdFilter::K50;
        let mut compatible_dsd_gain_comp = base.clone();
        compatible_dsd_gain_comp.compatible.dsd_gain_comp = false;
        let mut compatible_buffer = base.clone();
        compatible_buffer.compatible.buffer = buffer;

        let mut optimal_device = base.clone();
        optimal_device.optimal.device = Some("dac".to_string());
        let mut optimal_rate_fallback = base.clone();
        optimal_rate_fallback.optimal.rate_fallback = RateFallbackRule::Nearest;
        let mut optimal_src_filter = base.clone();
        optimal_src_filter.optimal.src_filter = SrcFilter::VerySlow;
        let mut optimal_dither = base.clone();
        optimal_dither.optimal.dither = Dither::Off;
        let mut optimal_volume_lock = base.clone();
        optimal_volume_lock.optimal.volume_lock = true;
        let mut optimal_dsd_filter = base.clone();
        optimal_dsd_filter.optimal.dsd_filter = DsdFilter::K24;
        let mut optimal_dsd_gain_comp = base.clone();
        optimal_dsd_gain_comp.optimal.dsd_gain_comp = false;
        let mut optimal_buffer = base.clone();
        optimal_buffer.optimal.buffer = buffer;
        let mut optimal_device_buffer = base.clone();
        optimal_device_buffer.optimal.device_buffer = DeviceBuffer::Ms200;

        let mut strict_device = base.clone();
        strict_device.strict.device = Some("dac".to_string());
        let mut strict_buffer = base.clone();
        strict_buffer.strict.buffer = buffer;
        let mut strict_device_buffer = base.clone();
        strict_device_buffer.strict.device_buffer = DeviceBuffer::Ms400;

        let cases = [
            (&compatible_device, ModeKind::Compatible, ParamId::Device),
            (&compatible_fixed_rate, ModeKind::Compatible, ParamId::FixedRate),
            (&compatible_src_filter, ModeKind::Compatible, ParamId::SrcFilter),
            (&compatible_dither, ModeKind::Compatible, ParamId::DitherKind),
            (&compatible_dsd_filter, ModeKind::Compatible, ParamId::DsdFilter),
            (&compatible_dsd_gain_comp, ModeKind::Compatible, ParamId::DsdGainComp),
            (&compatible_buffer, ModeKind::Compatible, ParamId::Buffer),
            (&optimal_device, ModeKind::Optimal, ParamId::Device),
            (&optimal_rate_fallback, ModeKind::Optimal, ParamId::RateFallback),
            (&optimal_src_filter, ModeKind::Optimal, ParamId::SrcFilter),
            (&optimal_dither, ModeKind::Optimal, ParamId::DitherKind),
            (&optimal_volume_lock, ModeKind::Optimal, ParamId::VolumeLock),
            (&optimal_dsd_filter, ModeKind::Optimal, ParamId::DsdFilter),
            (&optimal_dsd_gain_comp, ModeKind::Optimal, ParamId::DsdGainComp),
            (&optimal_buffer, ModeKind::Optimal, ParamId::Buffer),
            (&optimal_device_buffer, ModeKind::Optimal, ParamId::DeviceBuffer),
            (&strict_device, ModeKind::Strict, ParamId::Device),
            (&strict_buffer, ModeKind::Strict, ParamId::Buffer),
            (&strict_device_buffer, ModeKind::Strict, ParamId::DeviceBuffer),
        ];

        for (changed, mode, id) in cases {
            let diff = base.diff(changed);
            assert_eq!(diff.changed.len(), 1, "{mode:?}/{id:?}: ожидался ровно один изменённый параметр");
            assert_eq!(diff.changed[0], (mode, id), "{mode:?}/{id:?}: неверный ParamId в diff");
        }
    }

    /// И-Р26, §6.18: среди изменений активного режима выбирается действие с
    /// наибольшим эффектом — `SwitchDevice` важнее `ReopenAtPosition` и `Memory`.
    #[test]
    fn strongest_picks_switch_device_over_others() {
        let diff = ModeSettingsDiff {
            changed: SmallVec::from_slice(&[
                (ModeKind::Optimal, ParamId::SrcFilter),
                (ModeKind::Optimal, ParamId::Device),
                (ModeKind::Optimal, ParamId::DsdGainComp),
            ]),
        };
        assert_eq!(diff.strongest(ModeKind::Optimal), Some(ApplyKind::SwitchDevice));
    }

    /// Параметр без влияния на поток (`ApplyKind::Memory`) один в наборе —
    /// результат `Memory` (§6.18).
    #[test]
    fn strongest_memory_only_change() {
        let diff = ModeSettingsDiff {
            changed: SmallVec::from_slice(&[(ModeKind::Strict, ParamId::Access)]),
        };
        assert_eq!(diff.strongest(ModeKind::Strict), Some(ApplyKind::Memory));
    }

    /// И-Р26: изменения параметра неактивного режима не считаются — `None`,
    /// переоткрытия нет.
    #[test]
    fn strongest_ignores_inactive_mode() {
        let diff = ModeSettingsDiff {
            changed: SmallVec::from_slice(&[(ModeKind::Compatible, ParamId::Device)]),
        };
        assert_eq!(diff.strongest(ModeKind::Optimal), None);
    }
}
