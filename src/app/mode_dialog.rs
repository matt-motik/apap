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
    buffers_allowed, BufferMs, DeviceBuffer, Dither, DsdAboveDac, DsdFilter, ModeKind, ModeSettings,
    RateFallbackRule, SrcFilter, ValueBlocker,
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

/// Пункты списка «Фильтр SRC» в порядке отображения: единственный источник
/// соответствия индекс ↔ `SrcFilter` для `build_rows` и `apply_edit` (ТЗ-94,
/// ТЗ-130, §6.27). `None` — синтетический `NotImplemented`-пункт минимальной
/// фазы, непредставимый в `SrcFilter` (И-Т19).
const SRC_FILTER_OPTIONS: &[(&str, Option<SrcFilter>)] = &[
    ("Крутой", Some(SrcFilter::Steep)),
    ("Медленный", Some(SrcFilter::Slow)),
    ("Очень медленный", Some(SrcFilter::VerySlow)),
    ("Крутой, короткая задержка", None),
    ("Медленный, короткая задержка", None),
];

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
    let options = SRC_FILTER_OPTIONS
        .iter()
        .map(|(opt_label, variant)| RowOption {
            label: opt_label,
            enabled: variant.is_some(),
            hint: if variant.is_some() { String::new() } else { "пока не реализовано".to_string() },
        })
        .collect();
    let selected = SRC_FILTER_OPTIONS.iter().position(|(_, v)| *v == Some(current)).unwrap_or(0);
    DialogRow {
        param: ParamId::SrcFilter,
        label,
        kind: RowKind::List { options, selected },
        enabled: true,
        note: String::new(),
    }
}

/// Пункты списка «Буфер устройства» в порядке отображения: единственный
/// источник соответствия индекс ↔ `DeviceBuffer` для `build_rows` и
/// `apply_edit` (ТЗ-94, ТЗ-131, §6.27).
const DEVICE_BUFFER_OPTIONS: &[DeviceBuffer] =
    &[DeviceBuffer::Ms20, DeviceBuffer::Ms40, DeviceBuffer::Ms100, DeviceBuffer::Ms200, DeviceBuffer::Ms400];

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
    let mut options = Vec::with_capacity(DEVICE_BUFFER_OPTIONS.len());
    let mut selected = 0;
    for (i, candidate) in DEVICE_BUFFER_OPTIONS.iter().enumerate() {
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

/// Пункты списка «Фильтр DSD → PCM» в порядке отображения: единственный
/// источник соответствия индекс ↔ `DsdFilter` для `build_rows` и
/// `apply_edit` (ТЗ-94, ТЗ-128, §6.27).
const DSD_FILTER_OPTIONS: &[(&str, DsdFilter)] =
    &[("24 кГц", DsdFilter::K24), ("30 кГц", DsdFilter::K30), ("50 кГц", DsdFilter::K50)];

/// Список «Фильтр DSD → PCM» (ТЗ-128).
fn row_dsd_filter(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let current = match mode {
        ModeKind::Compatible => settings.compatible.dsd_filter,
        ModeKind::Optimal => settings.optimal.dsd_filter,
        ModeKind::Strict => DsdFilter::default(),
    };
    let options = DSD_FILTER_OPTIONS
        .iter()
        .map(|(opt_label, _)| RowOption { label: opt_label, enabled: true, hint: String::new() })
        .collect();
    let selected = DSD_FILTER_OPTIONS.iter().position(|(_, v)| *v == current).unwrap_or(0);
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

/// Пункты списка «Политика несовпадения частоты» в порядке отображения:
/// единственный источник соответствия индекс ↔ `RateFallbackRule` для
/// `build_rows` и `apply_edit` (ТЗ-94, §6.27).
const RATE_FALLBACK_OPTIONS: &[(&str, RateFallbackRule)] = &[
    ("Та же семья частот", RateFallbackRule::SameFamily),
    ("Ближайшая", RateFallbackRule::Nearest),
    ("Без понижения", RateFallbackRule::NoDownsample),
];

/// Список «Политика несовпадения частоты» (только `Optimal`).
fn row_rate_fallback(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let current = match mode {
        ModeKind::Optimal => settings.optimal.rate_fallback,
        ModeKind::Compatible | ModeKind::Strict => RateFallbackRule::default(),
    };
    let options = RATE_FALLBACK_OPTIONS
        .iter()
        .map(|(opt_label, _)| RowOption { label: opt_label, enabled: true, hint: String::new() })
        .collect();
    let selected = RATE_FALLBACK_OPTIONS.iter().position(|(_, v)| *v == current).unwrap_or(0);
    DialogRow {
        param: ParamId::RateFallback,
        label,
        kind: RowKind::List { options, selected },
        enabled: true,
        note: String::new(),
    }
}

/// Пункты списка «DSD выше возможностей ЦАП» в порядке отображения:
/// единственный источник соответствия индекс ↔ `DsdAboveDac` для
/// `build_rows` и `apply_edit` (ТЗ-34, ТЗ-94, §6.27). `None` — синтетический
/// `NotImplemented`-пункт «Ремодуляция», непредставимый в `DsdAboveDac`.
const DSD_ABOVE_DAC_OPTIONS: &[(&str, Option<DsdAboveDac>)] =
    &[("Преобразовать в PCM", Some(DsdAboveDac::ConvertToPcm)), ("Ремодуляция", None)];

/// Список «DSD выше возможностей ЦАП» с синтетическим `NotImplemented`-пунктом
/// «Ремодуляция», непредставимым в `DsdAboveDac` (ТЗ-34).
fn row_dsd_above_dac(id: ParamId, label: &'static str) -> DialogRow {
    let options = DSD_ABOVE_DAC_OPTIONS
        .iter()
        .map(|(opt_label, variant)| RowOption {
            label: opt_label,
            enabled: variant.is_some(),
            hint: if variant.is_some() { String::new() } else { "пока не реализовано".to_string() },
        })
        .collect();
    DialogRow { param: id, label, kind: RowKind::List { options, selected: 0 }, enabled: true, note: String::new() }
}

/// Значение, которое пользователь выставил в редактируемой строке диалога:
/// индекс выбранного пункта `RowKind::List`, положение `RowKind::Toggle` или
/// число `RowKind::Slider` (§6.27, ТЗ-94).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditValue {
    Index(usize),
    Toggle(bool),
    Slider(u16),
}

/// Причина отказа применить правку строки диалога к черновику настроек
/// режима (§6.27, ТЗ-94).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditRejected {
    /// Параметр не `Availability::Configurable` в данном режиме по таблице
    /// `PARAMS` (ТЗ-95).
    NotEditable,
    /// Вид `EditValue` не подходит строке, индекс вне диапазона списка или
    /// выбранный пункт отключён (синтетический `NotImplemented`-вариант,
    /// например минимальная фаза `SrcFilter` или «Ремодуляция»).
    BadValue,
    /// ТЗ-94: итоговая пара (буфер декодирование → вывод, буфер устройства)
    /// не проходит `buffers_allowed` — недопустимое сочетание не применяется.
    BuffersNotAllowed,
}

/// Применить правку строки диалога к черновику `settings` (§6.27, ТЗ-94).
/// Индекс списка понимается в том же порядке, в котором пункты отдаёт
/// `build_rows` (общие таблицы `*_OPTIONS`). При любой ошибке `settings`
/// остаётся нетронутым — мутация происходит только на пути `Ok`.
pub(super) fn apply_edit(
    settings: &mut ModeSettings,
    mode: ModeKind,
    param: ParamId,
    value: EditValue,
) -> Result<(), EditRejected> {
    let descriptor = PARAMS.iter().find(|d| d.id == param).ok_or(EditRejected::NotEditable)?;
    if !matches!((descriptor.availability)(mode), Availability::Configurable) {
        return Err(EditRejected::NotEditable);
    }
    match param {
        ParamId::SrcFilter => apply_src_filter(settings, mode, value),
        ParamId::DeviceBuffer => apply_device_buffer(settings, mode, value),
        ParamId::DsdFilter => apply_dsd_filter(settings, mode, value),
        ParamId::DsdGainComp => apply_dsd_gain_comp(settings, mode, value),
        ParamId::DitherKind => apply_dither_kind(settings, mode, value),
        ParamId::VolumeLock => apply_volume_lock(settings, mode, value),
        ParamId::Buffer => apply_buffer(settings, mode, value),
        ParamId::RateFallback => apply_rate_fallback(settings, mode, value),
        ParamId::DsdAboveDac => apply_dsd_above_dac(settings, mode, value),
        _ => Err(EditRejected::NotEditable),
    }
}

/// Правка списка «Фильтр SRC» по `SRC_FILTER_OPTIONS` (ТЗ-94, ТЗ-130).
fn apply_src_filter(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Index(index) = value else { return Err(EditRejected::BadValue) };
    let (_, variant) = SRC_FILTER_OPTIONS.get(index).ok_or(EditRejected::BadValue)?;
    let variant = variant.ok_or(EditRejected::BadValue)?;
    match mode {
        ModeKind::Compatible => settings.compatible.src_filter = variant,
        ModeKind::Optimal => settings.optimal.src_filter = variant,
        ModeKind::Strict => return Err(EditRejected::NotEditable),
    }
    Ok(())
}

/// Правка списка «Буфер устройства» по `DEVICE_BUFFER_OPTIONS`: кандидат,
/// не проходящий `buffers_allowed` с текущим буфером плеера режима,
/// отклоняется без изменения `settings` (ТЗ-94, ТЗ-131).
fn apply_device_buffer(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Index(index) = value else { return Err(EditRejected::BadValue) };
    let candidate = *DEVICE_BUFFER_OPTIONS.get(index).ok_or(EditRejected::BadValue)?;
    let buffer = match mode {
        ModeKind::Optimal => settings.optimal.buffer,
        ModeKind::Strict => settings.strict.buffer,
        ModeKind::Compatible => return Err(EditRejected::NotEditable),
    };
    buffers_allowed(buffer, candidate).map_err(|_| EditRejected::BuffersNotAllowed)?;
    match mode {
        ModeKind::Optimal => settings.optimal.device_buffer = candidate,
        ModeKind::Strict => settings.strict.device_buffer = candidate,
        ModeKind::Compatible => return Err(EditRejected::NotEditable),
    }
    Ok(())
}

/// Правка списка «Фильтр DSD → PCM» по `DSD_FILTER_OPTIONS` (ТЗ-94, ТЗ-128).
fn apply_dsd_filter(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Index(index) = value else { return Err(EditRejected::BadValue) };
    let (_, variant) = DSD_FILTER_OPTIONS.get(index).ok_or(EditRejected::BadValue)?;
    match mode {
        ModeKind::Compatible => settings.compatible.dsd_filter = *variant,
        ModeKind::Optimal => settings.optimal.dsd_filter = *variant,
        ModeKind::Strict => return Err(EditRejected::NotEditable),
    }
    Ok(())
}

/// Правка переключателя «Компенсация уровня DSD → PCM +6 дБ» (ТЗ-94, ТЗ-129).
fn apply_dsd_gain_comp(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Toggle(on) = value else { return Err(EditRejected::BadValue) };
    match mode {
        ModeKind::Compatible => settings.compatible.dsd_gain_comp = on,
        ModeKind::Optimal => settings.optimal.dsd_gain_comp = on,
        ModeKind::Strict => return Err(EditRejected::NotEditable),
    }
    Ok(())
}

/// Правка переключателя «Дизеринг» — `on` ↔ `Dither::Tpdf` (ТЗ-94).
fn apply_dither_kind(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Toggle(on) = value else { return Err(EditRejected::BadValue) };
    let dither = if on { Dither::Tpdf } else { Dither::Off };
    match mode {
        ModeKind::Compatible => settings.compatible.dither = dither,
        ModeKind::Optimal => settings.optimal.dither = dither,
        ModeKind::Strict => return Err(EditRejected::NotEditable),
    }
    Ok(())
}

/// Правка переключателя «Фиксировать громкость на 100 %» — только `Optimal`
/// (ТЗ-94, ОВС-19).
fn apply_volume_lock(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Toggle(on) = value else { return Err(EditRejected::BadValue) };
    match mode {
        ModeKind::Optimal => {
            settings.optimal.volume_lock = on;
            Ok(())
        }
        ModeKind::Compatible | ModeKind::Strict => Err(EditRejected::NotEditable),
    }
}

/// Правка ползунка «Буфер декодирование → вывод»: значение проверяется
/// конструктором `BufferMs::new` (без `as`), затем — парой с буфером
/// устройства режима через `buffers_allowed` там, где он есть (ТЗ-94,
/// ТЗ-131).
fn apply_buffer(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Slider(ms) = value else { return Err(EditRejected::BadValue) };
    let buffer = BufferMs::new(ms).ok_or(EditRejected::BadValue)?;
    match mode {
        ModeKind::Compatible => {
            settings.compatible.buffer = buffer;
            Ok(())
        }
        ModeKind::Optimal => {
            buffers_allowed(buffer, settings.optimal.device_buffer).map_err(|_| EditRejected::BuffersNotAllowed)?;
            settings.optimal.buffer = buffer;
            Ok(())
        }
        ModeKind::Strict => {
            buffers_allowed(buffer, settings.strict.device_buffer).map_err(|_| EditRejected::BuffersNotAllowed)?;
            settings.strict.buffer = buffer;
            Ok(())
        }
    }
}

/// Правка списка «Политика несовпадения частоты» по `RATE_FALLBACK_OPTIONS`
/// — только `Optimal` (ТЗ-94).
fn apply_rate_fallback(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Index(index) = value else { return Err(EditRejected::BadValue) };
    let (_, variant) = RATE_FALLBACK_OPTIONS.get(index).ok_or(EditRejected::BadValue)?;
    match mode {
        ModeKind::Optimal => {
            settings.optimal.rate_fallback = *variant;
            Ok(())
        }
        ModeKind::Compatible | ModeKind::Strict => Err(EditRejected::NotEditable),
    }
}

/// Правка списка «DSD выше возможностей ЦАП» по `DSD_ABOVE_DAC_OPTIONS` —
/// только `Optimal`; синтетический пункт «Ремодуляция» отклоняется (ТЗ-34,
/// ТЗ-94).
fn apply_dsd_above_dac(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Index(index) = value else { return Err(EditRejected::BadValue) };
    let (_, variant) = DSD_ABOVE_DAC_OPTIONS.get(index).ok_or(EditRejected::BadValue)?;
    let variant = variant.ok_or(EditRejected::BadValue)?;
    match mode {
        ModeKind::Optimal => {
            settings.optimal.dsd_above_dac = variant;
            Ok(())
        }
        ModeKind::Compatible | ModeKind::Strict => Err(EditRejected::NotEditable),
    }
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

    /// §6.27, ТЗ-94: правка пункта списка меняет значение черновика, и
    /// `build_rows` затем отражает новый выбранный пункт.
    #[test]
    fn valid_list_edit_changes_value_and_is_reflected_in_build_rows() {
        let mut settings = ModeSettings::default();
        apply_edit(&mut settings, ModeKind::Compatible, ParamId::SrcFilter, EditValue::Index(2))
            .expect("Index(2) — пункт «Очень медленный», допустимая правка");
        assert_eq!(settings.compatible.src_filter, SrcFilter::VerySlow);

        let rows = build_rows(ModeKind::Compatible, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::SrcFilter).expect("SrcFilter row must exist");
        let RowKind::List { selected, .. } = &row.kind else { panic!("expected List") };
        assert_eq!(*selected, 2);
    }

    /// ТЗ-94: буфер устройства, не проходящий `buffers_allowed` с текущим
    /// буфером плеера, отклоняется, `settings` не меняется.
    #[test]
    fn disallowed_device_buffer_edit_is_rejected_and_unchanged() {
        let mut settings = ModeSettings::default();
        settings.optimal.buffer = BufferMsForTest::ms100();
        let before = settings.clone();

        let result = apply_edit(&mut settings, ModeKind::Optimal, ParamId::DeviceBuffer, EditValue::Index(4));

        assert_eq!(result, Err(EditRejected::BuffersNotAllowed));
        assert_eq!(settings, before);
    }

    /// §6.27: синтетический `NotImplemented`-пункт («Крутой, короткая
    /// задержка») непредставим в `SrcFilter` — правка отклоняется, `settings`
    /// не меняется.
    #[test]
    fn not_implemented_option_edit_is_rejected_and_unchanged() {
        let mut settings = ModeSettings::default();
        let before = settings.clone();

        let result = apply_edit(&mut settings, ModeKind::Compatible, ParamId::SrcFilter, EditValue::Index(3));

        assert_eq!(result, Err(EditRejected::BadValue));
        assert_eq!(settings, before);
    }

    /// ТЗ-95: «Политика несовпадения частоты» — `FixedByMode` в Совместимом
    /// режиме, правка отклоняется как `NotEditable`.
    #[test]
    fn fixed_by_mode_param_edit_is_rejected_as_not_editable() {
        let mut settings = ModeSettings::default();
        let before = settings.clone();

        let result = apply_edit(&mut settings, ModeKind::Compatible, ParamId::RateFallback, EditValue::Index(1));

        assert_eq!(result, Err(EditRejected::NotEditable));
        assert_eq!(settings, before);
    }
}
