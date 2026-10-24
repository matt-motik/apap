//! Русские подписи параметров тракта `ParamId` для диалога настроек режима
//! (T1.0-B8; ТЗ-95, §2.2) — перенесено из
//! `src/settings/params.rs::param_label` байт-в-байт (решение пользователя
//! 2026-10-24), без изменения содержания.

use crate::settings::params::ParamId;

/// Русская подпись параметра для UI (ТЗ-95, §2.2). `label_key` в `PARAMS`
/// (`src/settings/params.rs`) — служебный английский идентификатор, а не
/// текст для пользователя.
pub fn param_label(id: ParamId) -> &'static str {
    match id {
        ParamId::Access => "Доступ к устройству",
        ParamId::Device => "Устройство",
        ParamId::Lock => "Монопольный захват",
        ParamId::OutputRate => "Частота вывода",
        ParamId::FixedRate => "Фиксированная частота",
        ParamId::RateFallback => "Политика несовпадения частоты",
        ParamId::Src => "Ресэмплинг (SRC)",
        ParamId::SrcFilter => "Фильтр SRC",
        ParamId::SampleFormat => "Формат сэмплов",
        ParamId::ZeroPad => "Дополнение нулями",
        ParamId::Truncation => "Усечение разрядности",
        ParamId::DitherApplied => "Применение дизеринга",
        ParamId::DitherKind => "Дизеринг (TPDF)",
        ParamId::Channels => "Каналы",
        ParamId::Downmix => "Даунмикс",
        ParamId::MonoToStereo => "Моно → стерео",
        ParamId::ChannelPad => "Дополнение каналов",
        ParamId::FloatSource => "Источник float",
        ParamId::Volume => "Громкость",
        ParamId::Mute => "Тишина (mute)",
        ParamId::VolumeLock => "Фиксировать громкость на 100 %",
        ParamId::Equalizer => "Эквалайзер",
        ParamId::DsdNative => "Native DSD",
        ParamId::DsdDop => "DSD через DoP",
        ParamId::DsdToPcm => "DSD → PCM",
        ParamId::DsdAboveDac => "DSD выше возможностей ЦАП",
        ParamId::DsdPcmParams => "Параметры DSD → PCM",
        ParamId::DsdFilter => "Фильтр DSD → PCM",
        ParamId::DsdGainComp => "Компенсация уровня DSD → PCM +6 дБ",
        ParamId::OnIncompatible => "При несовместимости",
        ParamId::OnBadFile => "При повреждённом файле",
        ParamId::OnDeviceLost => "При потере устройства",
        ParamId::Buffer => "Буфер декодирование → вывод",
        ParamId::DeviceBuffer => "Буфер устройства",
        ParamId::RtPriority => "Приоритет реального времени",
        ParamId::DeviceDop => "ЦАП понимает DoP",
        ParamId::Badge => "Бейдж",
        ParamId::Test => "Тест",
        ParamId::Md5 => "Проверка MD5",
        ParamId::UnderrunXrun => "Underrun/xrun",
    }
}
