//! Чистый построитель строк диалога настроек режима (§6.27, ТЗ-95,
//! ТЗ-128…ТЗ-131, И-Т19).
//!
//! `build_rows` разворачивает таблицу `PARAMS` (§2.2) для одного режима в
//! список строк диалога: каждая строка следует правилу §6.27 — `Configurable`
//! активна и несёт текущее значение, `FixedByMode` неактивна и показывает
//! зафиксированное значение с пояснением «задано режимом» (ТЗ-95),
//! `Unavailable` неактивна с причиной, `NotImplemented` неактивна с
//! пояснением «пока не реализовано». Пункты «Крутой, короткая задержка» и
//! «Медленный, короткая задержка» списка `SrcFilter`, а также «Ремодуляция»
//! у `DsdAboveDac`, непредставимы в типах `playback.rs` и синтезируются
//! здесь как доп. `NotImplemented`-варианты (И-Т19, ТЗ-34, ТЗ-130). Список
//! «Буфер устройства» дисквалифицирует значения, не проходящие
//! `buffers_allowed` с текущим буфером плеера режима, подсказкой «увеличьте
//! буфер плеера до {N} мс» (ТЗ-94, ТЗ-131).
//!
//! Модуль не трогает Slint и не делает I/O — чистая функция модели. Подключение
//! к диалогу настроек делается в шаге 32 (AM1.0-8.4); до этого модуль не
//! используется, поэтому `#![allow(dead_code)]`.
#![allow(dead_code)]

use music_player_rs::settings::params::{Availability, ParamDescriptor, ParamId, PARAMS};
use music_player_rs::settings::playback::{
    buffers_allowed, DeviceBuffer, Dither, DsdFilter, ModeKind, ModeSettings, RateFallbackRule,
    SrcFilter, ValueBlocker,
};

/// Строка диалога настроек режима: параметр, подпись, тип виджета,
/// активность и пояснение (§6.27, ТЗ-95).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DialogRow {
    pub(super) param: ParamId,
    pub(super) label: &'static str,
    pub(super) kind: RowKind,
    pub(super) enabled: bool,
    pub(super) note: String,
}

/// Вид виджета строки. `Info` — строка без интерактивного элемента (текстовое
/// значение или пояснение): используется для `FixedByMode`/`Unavailable`/
/// `NotImplemented` строк и для параметров, не смоделированных в
/// `playback.rs` как редактируемое поле.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum RowKind {
    List { options: Vec<RowOption>, selected: usize },
    Toggle { on: bool },
    Slider { value: u16, min: u16, max: u16 },
    Info { text: String },
}

/// Один пункт списка строки `RowKind::List` (ТЗ-130, ТЗ-131).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct RowOption {
    pub(super) label: &'static str,
    pub(super) enabled: bool,
    pub(super) hint: String,
}

/// Построить строки диалога настроек режима `mode` по таблице `PARAMS` и
/// текущим значениям `settings` (§6.27, ТЗ-95).
pub(super) fn build_rows(mode: ModeKind, settings: &ModeSettings) -> Vec<DialogRow> {
    PARAMS.iter().map(|descriptor| build_row(descriptor, mode, settings)).collect()
}

fn build_row(descriptor: &ParamDescriptor, mode: ModeKind, settings: &ModeSettings) -> DialogRow {
    let label = label(descriptor.id);
    match (descriptor.availability)(mode) {
        Availability::Configurable => build_configurable_row(descriptor.id, mode, settings, label),
        Availability::FixedByMode { shown } => DialogRow {
            param: descriptor.id,
            label,
            kind: RowKind::Info { text: shown.to_string() },
            enabled: false,
            note: format!("{shown} — задано режимом"),
        },
        Availability::Unavailable { why } => DialogRow {
            param: descriptor.id,
            label,
            kind: RowKind::Info { text: why.to_string() },
            enabled: false,
            note: why.to_string(),
        },
        Availability::NotImplemented => DialogRow {
            param: descriptor.id,
            label,
            kind: RowKind::Info { text: "пока не реализовано".to_string() },
            enabled: false,
            note: "пока не реализовано".to_string(),
        },
    }
}

/// Строки с модельным редактируемым значением (ТЗ-128…ТЗ-131). Параметры,
/// `Configurable` в каком-либо режиме, но не смоделированные здесь явно
/// (например, «Устройство», «Тест»), получают нейтральную `Info`-строку —
/// их редактор появится отдельным шагом, когда для них будет каталог
/// значений (устройства, действия).
fn build_configurable_row(id: ParamId, mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    match id {
        ParamId::SrcFilter => row_src_filter(mode, settings, label),
        ParamId::DeviceBuffer => row_device_buffer(mode, settings, label),
        ParamId::DsdFilter => row_dsd_filter(mode, settings, label),
        ParamId::DsdGainComp => row_dsd_gain_comp(mode, settings, label),
        ParamId::DitherKind => row_dither_kind(mode, settings, label),
        ParamId::VolumeLock => row_volume_lock(mode, settings, label),
        ParamId::Buffer => row_buffer(mode, settings, label),
        ParamId::RateFallback => row_rate_fallback(mode, settings, label),
        ParamId::DsdAboveDac => row_dsd_above_dac(id, label),
        _ => DialogRow {
            param: id,
            label,
            kind: RowKind::Info { text: String::new() },
            enabled: true,
            note: String::new(),
        },
    }
}

/// Список «Фильтр SRC» с двумя синтетическими `NotImplemented`-пунктами
/// минимальной фазы, непредставимыми в `SrcFilter` (И-Т19, ТЗ-130).
fn row_src_filter(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    // `Strict` не бывает `Configurable` для `SrcFilter` — сюда не попадает;
    // запасное значение ничего не отображает (нет `unwrap`/`panic`).
    let current = match mode {
        ModeKind::Compatible => settings.compatible.src_filter,
        ModeKind::Optimal => settings.optimal.src_filter,
        ModeKind::Strict => SrcFilter::default(),
    };
    let options = vec![
        RowOption { label: "Крутой", enabled: true, hint: String::new() },
        RowOption { label: "Медленный", enabled: true, hint: String::new() },
        RowOption { label: "Очень медленный", enabled: true, hint: String::new() },
        RowOption {
            label: "Крутой, короткая задержка",
            enabled: false,
            hint: "пока не реализовано".to_string(),
        },
        RowOption {
            label: "Медленный, короткая задержка",
            enabled: false,
            hint: "пока не реализовано".to_string(),
        },
    ];
    let selected = match current {
        SrcFilter::Steep => 0,
        SrcFilter::Slow => 1,
        SrcFilter::VerySlow => 2,
    };
    DialogRow {
        param: ParamId::SrcFilter,
        label,
        kind: RowKind::List { options, selected },
        enabled: true,
        note: String::new(),
    }
}

/// Список «Буфер устройства»: пункт, не проходящий `buffers_allowed` с
/// текущим буфером плеера режима, неактивен с подсказкой (ТЗ-94, ТЗ-131).
fn row_device_buffer(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    // `Compatible` не бывает `Configurable` для `DeviceBuffer` — сюда не
    // попадает; запасная пара ничего не отображает (нет `unwrap`/`panic`).
    let (buffer, device_buffer) = match mode {
        ModeKind::Optimal => (settings.optimal.buffer, settings.optimal.device_buffer),
        ModeKind::Strict => (settings.strict.buffer, settings.strict.device_buffer),
        ModeKind::Compatible => (settings.optimal.buffer, settings.optimal.device_buffer),
    };
    let candidates = [
        DeviceBuffer::Ms20,
        DeviceBuffer::Ms40,
        DeviceBuffer::Ms100,
        DeviceBuffer::Ms200,
        DeviceBuffer::Ms400,
    ];
    let mut options = Vec::with_capacity(candidates.len());
    let mut selected = 0;
    for (i, candidate) in candidates.iter().enumerate() {
        if *candidate == device_buffer {
            selected = i;
        }
        let (enabled, hint) = match buffers_allowed(buffer, *candidate) {
            Ok(()) => (true, String::new()),
            Err(ValueBlocker::PlayerBufferTooSmall { need_ms }) => {
                (false, format!("увеличьте буфер плеера до {need_ms} мс"))
            }
            Err(ValueBlocker::DeviceBufferTooLarge { max_ms }) => {
                (false, format!("уменьшите буфер устройства до {max_ms} мс"))
            }
        };
        options.push(RowOption { label: device_buffer_label(*candidate), enabled, hint });
    }
    DialogRow {
        param: ParamId::DeviceBuffer,
        label,
        kind: RowKind::List { options, selected },
        enabled: true,
        note: String::new(),
    }
}

fn device_buffer_label(d: DeviceBuffer) -> &'static str {
    match d {
        DeviceBuffer::Ms20 => "20 мс",
        DeviceBuffer::Ms40 => "40 мс",
        DeviceBuffer::Ms100 => "100 мс",
        DeviceBuffer::Ms200 => "200 мс",
        DeviceBuffer::Ms400 => "400 мс",
    }
}

/// Список «Фильтр DSD → PCM» (ТЗ-128).
fn row_dsd_filter(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let current = match mode {
        ModeKind::Compatible => settings.compatible.dsd_filter,
        ModeKind::Optimal => settings.optimal.dsd_filter,
        ModeKind::Strict => DsdFilter::default(),
    };
    let options = vec![
        RowOption { label: "24 кГц", enabled: true, hint: String::new() },
        RowOption { label: "30 кГц", enabled: true, hint: String::new() },
        RowOption { label: "50 кГц", enabled: true, hint: String::new() },
    ];
    let selected = match current {
        DsdFilter::K24 => 0,
        DsdFilter::K30 => 1,
        DsdFilter::K50 => 2,
    };
    DialogRow {
        param: ParamId::DsdFilter,
        label,
        kind: RowKind::List { options, selected },
        enabled: true,
        note: String::new(),
    }
}

/// Переключатель «Компенсация уровня DSD → PCM +6 дБ» (ТЗ-129).
fn row_dsd_gain_comp(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let on = match mode {
        ModeKind::Compatible => settings.compatible.dsd_gain_comp,
        ModeKind::Optimal => settings.optimal.dsd_gain_comp,
        ModeKind::Strict => false,
    };
    DialogRow { param: ParamId::DsdGainComp, label, kind: RowKind::Toggle { on }, enabled: true, note: String::new() }
}

/// Переключатель «Дизеринг» (`Dither::Tpdf`/`Off`).
fn row_dither_kind(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let dither = match mode {
        ModeKind::Compatible => settings.compatible.dither,
        ModeKind::Optimal => settings.optimal.dither,
        ModeKind::Strict => Dither::default(),
    };
    DialogRow {
        param: ParamId::DitherKind,
        label,
        kind: RowKind::Toggle { on: dither == Dither::Tpdf },
        enabled: true,
        note: String::new(),
    }
}

/// Переключатель «Фиксировать громкость на 100 %» (только `Optimal`).
fn row_volume_lock(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let on = match mode {
        ModeKind::Optimal => settings.optimal.volume_lock,
        ModeKind::Compatible | ModeKind::Strict => false,
    };
    DialogRow { param: ParamId::VolumeLock, label, kind: RowKind::Toggle { on }, enabled: true, note: String::new() }
}

/// Ползунок «Буфер декодирование → вывод» в мс (ТЗ-131).
fn row_buffer(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let buffer = match mode {
        ModeKind::Compatible => settings.compatible.buffer,
        ModeKind::Optimal => settings.optimal.buffer,
        ModeKind::Strict => settings.strict.buffer,
    };
    DialogRow {
        param: ParamId::Buffer,
        label,
        kind: RowKind::Slider { value: buffer.get(), min: 100, max: 10_000 },
        enabled: true,
        note: String::new(),
    }
}

/// Список «Политика несовпадения частоты» (только `Optimal`).
fn row_rate_fallback(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let current = match mode {
        ModeKind::Optimal => settings.optimal.rate_fallback,
        ModeKind::Compatible | ModeKind::Strict => RateFallbackRule::default(),
    };
    let options = vec![
        RowOption { label: "Та же семья частот", enabled: true, hint: String::new() },
        RowOption { label: "Ближайшая", enabled: true, hint: String::new() },
        RowOption { label: "Без понижения", enabled: true, hint: String::new() },
    ];
    let selected = match current {
        RateFallbackRule::SameFamily => 0,
        RateFallbackRule::Nearest => 1,
        RateFallbackRule::NoDownsample => 2,
    };
    DialogRow {
        param: ParamId::RateFallback,
        label,
        kind: RowKind::List { options, selected },
        enabled: true,
        note: String::new(),
    }
}

/// Список «DSD выше возможностей ЦАП» с синтетическим `NotImplemented`-пунктом
/// «Ремодуляция», непредставимым в `DsdAboveDac` (ТЗ-34).
fn row_dsd_above_dac(id: ParamId, label: &'static str) -> DialogRow {
    let options = vec![
        RowOption { label: "Преобразовать в PCM", enabled: true, hint: String::new() },
        RowOption { label: "Ремодуляция", enabled: false, hint: "пока не реализовано".to_string() },
    ];
    DialogRow { param: id, label, kind: RowKind::List { options, selected: 0 }, enabled: true, note: String::new() }
}

/// Русская подпись параметра (ТЗ-95, §2.2). `label_key` в `PARAMS` —
/// служебный английский идентификатор, а не текст для UI.
fn label(id: ParamId) -> &'static str {
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
        ParamId::Badge => "Бейдж",
        ParamId::Test => "Тест",
        ParamId::Md5 => "Проверка MD5",
        ParamId::UnderrunXrun => "Underrun/xrun",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn fixed_by_mode_row_is_disabled_with_note() {
        let settings = ModeSettings::default();
        let rows = build_rows(ModeKind::Compatible, &settings);
        let access = rows.iter().find(|r| r.param == ParamId::Access).expect("Access row must exist");
        assert!(!access.enabled);
        assert!(access.note.contains("задано режимом"));
    }

    #[test]
    fn device_buffer_option_disallowed_by_buffers_allowed_is_disabled_with_hint() {
        let mut settings = ModeSettings::default();
        settings.optimal.buffer = BufferMsForTest::ms100();
        let rows = build_rows(ModeKind::Optimal, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::DeviceBuffer).expect("DeviceBuffer row must exist");
        let RowKind::List { options, .. } = &row.kind else { panic!("expected List") };
        let ms400 = options.iter().find(|o| o.label == "400 мс").expect("400 мс option must exist");
        assert!(!ms400.enabled);
        assert!(ms400.hint.contains("буфер плеера"));
    }

    #[test]
    fn not_implemented_src_filter_option_is_disabled() {
        let settings = ModeSettings::default();
        let rows = build_rows(ModeKind::Compatible, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::SrcFilter).expect("SrcFilter row must exist");
        let RowKind::List { options, .. } = &row.kind else { panic!("expected List") };
        let short_latency = options
            .iter()
            .find(|o| o.label == "Крутой, короткая задержка")
            .expect("short-latency option must exist");
        assert!(!short_latency.enabled);
        assert_eq!(short_latency.hint, "пока не реализовано");
    }

    #[test]
    fn editable_row_reflects_current_setting_value() {
        let mut settings = ModeSettings::default();
        settings.compatible.src_filter = SrcFilter::VerySlow;
        let rows = build_rows(ModeKind::Compatible, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::SrcFilter).expect("SrcFilter row must exist");
        let RowKind::List { selected, .. } = &row.kind else { panic!("expected List") };
        assert_eq!(*selected, 2);
    }

    #[test]
    fn strict_and_compatible_differ_in_availability_for_src_filter() {
        let settings = ModeSettings::default();
        let compatible_row = build_rows(ModeKind::Compatible, &settings)
            .into_iter()
            .find(|r| r.param == ParamId::SrcFilter)
            .expect("SrcFilter row must exist");
        let strict_row = build_rows(ModeKind::Strict, &settings)
            .into_iter()
            .find(|r| r.param == ParamId::SrcFilter)
            .expect("SrcFilter row must exist");
        assert!(compatible_row.enabled);
        assert!(!strict_row.enabled);
    }

    /// Вспомогательный конструктор буфера плеера для теста: 100 мс
    /// попадают в допустимый диапазон `BufferMs` и не проходят
    /// `buffers_allowed` с `DeviceBuffer::Ms400` (нужны >= 800 мс).
    struct BufferMsForTest;
    impl BufferMsForTest {
        fn ms100() -> music_player_rs::settings::playback::BufferMs {
            music_player_rs::settings::playback::BufferMs::new(100)
                .expect("100 мс входит в допустимый диапазон BufferMs")
        }
    }
}
