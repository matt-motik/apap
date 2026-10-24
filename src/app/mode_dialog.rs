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
//! здесь как доп. `NotImplemented`-варианты (И-Т19, ТЗ-34, ТЗ-130). Пара
//! «Буфер устройства» / «Буфер декодирование→вывод» не отказывает
//! недопустимому сочетанию, а связывается автоподъёмом: повышение буфера
//! устройства поднимает черновой буфер плеера до `buffer_floor_ms`
//! (`raise_buffer_for_device`), а значение буфера плеера ниже этой границы
//! клампится до неё — связь односторонняя, уменьшение буфера устройства
//! назад не опускает (ТЗ-94, ТЗ-131, §6.27, решение пользователя
//! 2026-10-24). `build_rows` принимает второй аргумент — сохранённые
//! настройки (`saved`) — чтобы `row_buffer` отличила «буфер плеера поднят
//! последней правкой буфера устройства» от «пользователь сам поставил его
//! на границу или выше»: строка «Поднят автоматически до N мс» показывается
//! только в первом случае (AM1.0-B35, step 7).
//!
//! Построенные строки отображаются панелью настроек режима в Slint
//! (`ui/mode_params.slint`, `ModeParamRow`): `to_param_row` переводит одну
//! `DialogRow` в одну строку панели, подключение сделано в `mod.rs`
//! (§6.18, §6.27, ТЗ-41, ТЗ-94, ОВС-14).

use music_player_rs::i18n;
use music_player_rs::settings::params::{param_label, Availability, ParamDescriptor, ParamId, PARAMS};
use music_player_rs::settings::playback::{
    buffer_floor_ms, raise_buffer_for_device, BufferMs, DeviceBuffer, DeviceOpts, Dither, DsdAboveDac, DsdFilter,
    ModeKind, ModeSettings, RateFallbackRule, SrcFilter, ValueBlocker,
};
use slint::{ModelRc, SharedString};

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
/// текущим черновым значениям `settings` (§6.27, ТЗ-95). `saved` —
/// сохранённые настройки (не черновик): `row_buffer` сравнивает их с
/// `settings`, чтобы показать строку «Поднят автоматически до N мс» только
/// когда границу буфера плеера поднял сам диалог, а не пользователь
/// (AM1.0-B35, step 7).
pub(super) fn build_rows(mode: ModeKind, settings: &ModeSettings, saved: &ModeSettings) -> Vec<DialogRow> {
    PARAMS.iter().map(|descriptor| build_row(descriptor, mode, settings, saved)).collect()
}

/// Индекс параметра в таблице `PARAMS` — единственный источник соответствия
/// `ParamId` ↔ `param: int` строки `ModeParamRow` в обе стороны (§6.18). `-1`,
/// если параметр не найден (не должно происходить — `PARAMS` статична).
pub(super) fn param_index(id: ParamId) -> i32 {
    PARAMS.iter().position(|d| d.id == id).map_or(-1, |i| i as i32)
}

/// Обратное соответствие: параметр по его индексу в `PARAMS`, как его прислал
/// обработчик колбэка панели (`param: int` из Slint, §6.18). `None` — индекс
/// вне таблицы, колбэк его игнорирует.
pub(super) fn param_at(index: i32) -> Option<ParamId> {
    usize::try_from(index).ok().and_then(|i| PARAMS.get(i)).map(|d| d.id)
}

/// `DialogRow` → строка панели `ModeParamRow` в Slint (§6.18, §6.27, ТЗ-41,
/// ТЗ-94, ТЗ-95). У недоступного пункта списка (`RowOption::enabled ==
/// false`) подсказка дописывается в текст подписи — у `ModeParamRow.options`
/// нет отдельного поля доступности пункта.
pub(super) fn to_param_row(row: &DialogRow) -> super::ModeParamRow {
    let mut checked = false;
    let mut value = 0.0_f32;
    let mut min = 0.0_f32;
    let mut max = 0.0_f32;
    let mut selected = 0_i32;
    let mut options: Vec<SharedString> = Vec::new();
    let kind = match &row.kind {
        RowKind::List { options: opts, selected: sel } => {
            selected = *sel as i32;
            options = opts.iter().map(option_label).collect();
            0
        }
        RowKind::Toggle { on } => {
            checked = *on;
            1
        }
        RowKind::Slider { value: v, min: lo, max: hi } => {
            value = f32::from(*v);
            min = f32::from(*lo);
            max = f32::from(*hi);
            2
        }
        RowKind::Info { .. } => 3,
    };
    super::ModeParamRow {
        param: param_index(row.param),
        label: SharedString::from(row.label),
        kind,
        enabled: row.enabled,
        note: SharedString::from(row.note.as_str()),
        options: ModelRc::from(options.as_slice()),
        selected,
        checked,
        value,
        min,
        max,
    }
}

/// Подпись пункта списка: у отключённого пункта подсказка дописывается в
/// скобках (ТЗ-94, ТЗ-130, ТЗ-131).
fn option_label(opt: &RowOption) -> SharedString {
    if opt.enabled || opt.hint.is_empty() {
        SharedString::from(opt.label)
    } else {
        SharedString::from(format!("{} ({})", opt.label, opt.hint))
    }
}

fn build_row(descriptor: &ParamDescriptor, mode: ModeKind, settings: &ModeSettings, saved: &ModeSettings) -> DialogRow {
    let label = param_label(descriptor.id);
    match (descriptor.availability)(mode) {
        Availability::Configurable => build_configurable_row(descriptor.id, mode, settings, saved, label),
        Availability::FixedByMode { shown } => DialogRow {
            param: descriptor.id,
            label,
            kind: RowKind::Info { text: shown.to_string() },
            enabled: false,
            note: i18n::mode_dialog_fixed_by_mode(shown),
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
            kind: RowKind::Info { text: i18n::MODE_DIALOG_NOT_IMPLEMENTED.to_string() },
            enabled: false,
            note: i18n::MODE_DIALOG_NOT_IMPLEMENTED.to_string(),
        },
    }
}

/// Строки с модельным редактируемым значением (ТЗ-128…ТЗ-131). Параметры,
/// `Configurable` в каком-либо режиме, но не смоделированные здесь явно
/// (например, «Устройство», «Тест»), получают нейтральную `Info`-строку —
/// их редактор появится отдельным шагом, когда для них будет каталог
/// значений (устройства, действия).
fn build_configurable_row(
    id: ParamId,
    mode: ModeKind,
    settings: &ModeSettings,
    saved: &ModeSettings,
    label: &'static str,
) -> DialogRow {
    match id {
        ParamId::SrcFilter => row_src_filter(mode, settings, label),
        ParamId::DeviceBuffer => row_device_buffer(mode, settings, label),
        ParamId::DsdFilter => row_dsd_filter(mode, settings, label),
        ParamId::DsdGainComp => row_dsd_gain_comp(mode, settings, label),
        ParamId::DitherKind => row_dither_kind(mode, settings, label),
        ParamId::VolumeLock => row_volume_lock(mode, settings, label),
        ParamId::Buffer => row_buffer(mode, settings, saved, label),
        ParamId::RateFallback => row_rate_fallback(mode, settings, label),
        ParamId::DsdAboveDac => row_dsd_above_dac(id, label),
        ParamId::DeviceDop => row_device_dop(mode, settings, label),
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
    (i18n::SRC_FILTER_STEEP, Some(SrcFilter::Steep)),
    (i18n::SRC_FILTER_SLOW, Some(SrcFilter::Slow)),
    (i18n::SRC_FILTER_VERY_SLOW, Some(SrcFilter::VerySlow)),
    (i18n::SRC_FILTER_STEEP_SHORT_LATENCY, None),
    (i18n::SRC_FILTER_SLOW_SHORT_LATENCY, None),
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
            hint: if variant.is_some() { String::new() } else { i18n::MODE_DIALOG_NOT_IMPLEMENTED.to_string() },
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

/// Список «Буфер устройства»: пункт неактивен только если его граница
/// (`buffer_floor_ms`) не представима в `BufferMs` (100..=10 000 мс) —
/// резерв на будущее, при нынешних пяти значениях `DeviceBuffer` такого
/// пункта нет (ТЗ-94, ТЗ-131, §6.27, решение пользователя 2026-10-24).
/// Пункт больше не зависит от текущего чернового буфера плеера режима —
/// несовпадение разрешается автоподъёмом в `apply_device_buffer`, а не
/// отказом.
fn row_device_buffer(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    // `Compatible` не бывает `Configurable` для `DeviceBuffer` — сюда не
    // попадает; запасное значение ничего не отображает (нет `unwrap`/`panic`).
    let device_buffer = match mode {
        ModeKind::Optimal => settings.optimal.device_buffer,
        ModeKind::Strict => settings.strict.device_buffer,
        ModeKind::Compatible => DeviceBuffer::default(),
    };
    let mut options = Vec::with_capacity(DEVICE_BUFFER_OPTIONS.len());
    let mut selected = 0;
    for (i, candidate) in DEVICE_BUFFER_OPTIONS.iter().enumerate() {
        if *candidate == device_buffer {
            selected = i;
        }
        let (enabled, hint) = match BufferMs::new(buffer_floor_ms(*candidate)) {
            Some(_) => (true, String::new()),
            None => (false, ValueBlocker::DeviceBufferTooLarge { max_ms: candidate.ms() }.reason()),
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
        DeviceBuffer::Ms20 => i18n::DEVICE_BUFFER_20MS,
        DeviceBuffer::Ms40 => i18n::DEVICE_BUFFER_40MS,
        DeviceBuffer::Ms100 => i18n::DEVICE_BUFFER_100MS,
        DeviceBuffer::Ms200 => i18n::DEVICE_BUFFER_200MS,
        DeviceBuffer::Ms400 => i18n::DEVICE_BUFFER_400MS,
    }
}

/// Пункты списка «Фильтр DSD → PCM» в порядке отображения: единственный
/// источник соответствия индекс ↔ `DsdFilter` для `build_rows` и
/// `apply_edit` (ТЗ-94, ТЗ-128, §6.27).
const DSD_FILTER_OPTIONS: &[(&str, DsdFilter)] = &[
    (i18n::DSD_FILTER_24K, DsdFilter::K24),
    (i18n::DSD_FILTER_30K, DsdFilter::K30),
    (i18n::DSD_FILTER_50K, DsdFilter::K50),
];

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

/// Ползунок «Буфер декодирование → вывод» в мс (ТЗ-131, §6.27, решение
/// пользователя 2026-10-24). Нижняя граница в Оптимальном/Строгом —
/// `buffer_floor_ms(device_buffer)` (`max(100, 2·device)`); в Совместимом
/// `DeviceBuffer` не участвует — граница всегда 100, без строки. Под
/// ползунком Оптимального/Строгого режима постоянно видна строка текущей
/// границы (`buffer_floor_note`) — не только у края шкалы, как раньше.
/// Значение, ниже границы оказаться не может: `apply_buffer` и
/// `apply_device_buffer` клампят/поднимают черновой буфер плеера до неё
/// вместо отказа.
///
/// Вторая строка «Поднят автоматически до N мс» (ТЗ-131, §6.27,
/// `i18n::mode_dialog_buffer_auto_raised`, AM1.0-B35, step 7) добавляется,
/// когда текущее черновое значение равно границе `buffer_floor_ms`
/// **из-за автоподъёма**, а не выбрано пользователем: вместо отдельного
/// флага в `DialogDraft` признак выводится сравнением чернового `settings`
/// с сохранённым `saved` для того же режима — буфер устройства изменился,
/// буфер плеера выше сохранённого значения и равен текущей границе.
fn row_buffer(mode: ModeKind, settings: &ModeSettings, saved: &ModeSettings, label: &'static str) -> DialogRow {
    let (buffer, device_buffer) = buffer_and_device(mode, settings);
    let (saved_buffer, saved_device_buffer) = buffer_and_device(mode, saved);
    let min = device_buffer.map_or(100, buffer_floor_ms);
    let mut note = device_buffer.map(buffer_floor_note).unwrap_or_default();
    if let (Some(device_buffer), Some(saved_device_buffer)) = (device_buffer, saved_device_buffer) {
        let raised = device_buffer != saved_device_buffer
            && buffer.get() > saved_buffer.get()
            && buffer.get() == buffer_floor_ms(device_buffer);
        if raised {
            note.push('\n');
            note.push_str(&i18n::mode_dialog_buffer_auto_raised(buffer.get()));
        }
    }
    DialogRow {
        param: ParamId::Buffer,
        label,
        kind: RowKind::Slider { value: buffer.get(), min, max: 10_000 },
        enabled: true,
        note,
    }
}

/// Буфер плеера и (для Оптимального/Строгого) буфер устройства режима
/// `mode` в `settings` — общий разбор для `row_buffer`, вызывается и для
/// чернового, и для сохранённого `ModeSettings` (AM1.0-B35, step 7).
fn buffer_and_device(mode: ModeKind, settings: &ModeSettings) -> (BufferMs, Option<DeviceBuffer>) {
    match mode {
        ModeKind::Compatible => (settings.compatible.buffer, None),
        ModeKind::Optimal => (settings.optimal.buffer, Some(settings.optimal.device_buffer)),
        ModeKind::Strict => (settings.strict.buffer, Some(settings.strict.device_buffer)),
    }
}

/// Текст нижней границы ползунка «Буфер декодирование → вывод», всегда
/// видимый в Оптимальном/Строгом режиме (ТЗ-131, §6.27, решение
/// пользователя 2026-10-24): «минимум {N} мс», и когда границу поднимает
/// буфер устройства (`N > 100`) — пояснение «вдвое больше буфера устройства
/// ({d} мс)» (`i18n::mode_dialog_buffer_floor`/`_raised`, AM1.0-B35).
fn buffer_floor_note(device: DeviceBuffer) -> String {
    let floor = buffer_floor_ms(device);
    if floor > 100 {
        i18n::mode_dialog_buffer_floor_raised(floor, device.ms())
    } else {
        i18n::mode_dialog_buffer_floor(floor)
    }
}

/// Пункты списка «Политика несовпадения частоты» в порядке отображения:
/// единственный источник соответствия индекс ↔ `RateFallbackRule` для
/// `build_rows` и `apply_edit` (ТЗ-94, §6.27).
const RATE_FALLBACK_OPTIONS: &[(&str, RateFallbackRule)] = &[
    (i18n::RATE_FALLBACK_SAME_FAMILY, RateFallbackRule::SameFamily),
    (i18n::RATE_FALLBACK_NEAREST, RateFallbackRule::Nearest),
    (i18n::RATE_FALLBACK_NO_DOWNSAMPLE, RateFallbackRule::NoDownsample),
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
const DSD_ABOVE_DAC_OPTIONS: &[(&str, Option<DsdAboveDac>)] = &[
    (i18n::DSD_ABOVE_DAC_CONVERT_TO_PCM, Some(DsdAboveDac::ConvertToPcm)),
    (i18n::DSD_ABOVE_DAC_REMODULATION, None),
];

/// Список «DSD выше возможностей ЦАП» с синтетическим `NotImplemented`-пунктом
/// «Ремодуляция», непредставимым в `DsdAboveDac` (ТЗ-34).
fn row_dsd_above_dac(id: ParamId, label: &'static str) -> DialogRow {
    let options = DSD_ABOVE_DAC_OPTIONS
        .iter()
        .map(|(opt_label, variant)| RowOption {
            label: opt_label,
            enabled: variant.is_some(),
            hint: if variant.is_some() { String::new() } else { i18n::MODE_DIALOG_NOT_IMPLEMENTED.to_string() },
        })
        .collect();
    DialogRow { param: id, label, kind: RowKind::List { options, selected: 0 }, enabled: true, note: String::new() }
}

/// Переключатель «ЦАП понимает DoP» — Оптимальный/Строгий режим, устройство
/// режима берётся из `settings.optimal.device`/`settings.strict.device`
/// (ADR-25, ТЗ-145). Без выбранного устройства переключатель неактивен —
/// значение не к чему применить.
fn row_device_dop(mode: ModeKind, settings: &ModeSettings, label: &'static str) -> DialogRow {
    let device = match mode {
        ModeKind::Optimal => &settings.optimal.device,
        ModeKind::Strict => &settings.strict.device,
        // `Compatible` не бывает `Configurable` для `DeviceDop` — сюда не
        // попадает; запасная строка ничего не отображает (нет `unwrap`/`panic`).
        ModeKind::Compatible => {
            return DialogRow {
                param: ParamId::DeviceDop,
                label,
                kind: RowKind::Toggle { on: true },
                enabled: false,
                note: i18n::MODE_DIALOG_DEVICE_NOT_SELECTED.to_string(),
            };
        }
    };
    match device {
        Some(id) => DialogRow {
            param: ParamId::DeviceDop,
            label,
            kind: RowKind::Toggle { on: settings.device_opts(id).dop },
            enabled: true,
            note: String::new(),
        },
        None => DialogRow {
            param: ParamId::DeviceDop,
            label,
            kind: RowKind::Toggle { on: true },
            enabled: false,
            note: i18n::MODE_DIALOG_DEVICE_NOT_SELECTED.to_string(),
        },
    }
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
    /// Резерв на будущее (ТЗ-94, ТЗ-131, §6.27, решение пользователя
    /// 2026-10-24): граница `buffer_floor_ms` для правки вышла за допустимый
    /// диапазон `BufferMs` (100..=10 000 мс) — `raise_buffer_for_device`
    /// вернул `None`. Недостижимо при нынешних пяти значениях
    /// `DeviceBuffer` (`buffer_floor_ms(Ms400) = 800`); пара буферов больше
    /// не отказывает иначе — несовпадение разрешается автоподъёмом/клампом.
    /// Несёт причину (`ValueBlocker::DeviceBufferTooLarge`), чтобы
    /// обработчик мог показать конкретный текст вместо общей формулировки
    /// (AM1.0-B33).
    BuffersNotAllowed(ValueBlocker),
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
        ParamId::DeviceDop => apply_device_dop(settings, mode, value),
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

/// Правка списка «Буфер устройства» по `DEVICE_BUFFER_OPTIONS`: вместо
/// отказа поднимает черновой буфер плеера режима до
/// `buffer_floor_ms(candidate)`, если он ниже этой границы —
/// `raise_buffer_for_device` (ТЗ-94, ТЗ-131, §6.27, решение пользователя
/// 2026-10-24). Уменьшение буфера устройства уже поднятый буфер плеера
/// назад не опускает — односторонняя связь реализована в
/// `raise_buffer_for_device`. `BuffersNotAllowed` — резерв на будущее
/// (граница вне диапазона `BufferMs`), недостижимо при нынешних пяти
/// значениях `DeviceBuffer`.
fn apply_device_buffer(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Index(index) = value else { return Err(EditRejected::BadValue) };
    let candidate = *DEVICE_BUFFER_OPTIONS.get(index).ok_or(EditRejected::BadValue)?;
    let buffer = match mode {
        ModeKind::Optimal => settings.optimal.buffer,
        ModeKind::Strict => settings.strict.buffer,
        ModeKind::Compatible => return Err(EditRejected::NotEditable),
    };
    let raised = raise_buffer_for_device(buffer, candidate)
        .ok_or(EditRejected::BuffersNotAllowed(ValueBlocker::DeviceBufferTooLarge { max_ms: candidate.ms() }))?;
    match mode {
        ModeKind::Optimal => {
            settings.optimal.device_buffer = candidate;
            settings.optimal.buffer = raised;
        }
        ModeKind::Strict => {
            settings.strict.device_buffer = candidate;
            settings.strict.buffer = raised;
        }
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
/// конструктором `BufferMs::new` (без `as`), затем — границей буфера
/// устройства режима через `raise_buffer_for_device` там, где он есть:
/// значение ниже границы клампится до неё, а не отклоняется (ТЗ-94,
/// ТЗ-131, §6.27, решение пользователя 2026-10-24). `BuffersNotAllowed` —
/// резерв на будущее (граница вне диапазона `BufferMs`), недостижимо при
/// нынешних значениях `DeviceBuffer`.
fn apply_buffer(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Slider(ms) = value else { return Err(EditRejected::BadValue) };
    let buffer = BufferMs::new(ms).ok_or(EditRejected::BadValue)?;
    match mode {
        ModeKind::Compatible => {
            settings.compatible.buffer = buffer;
            Ok(())
        }
        ModeKind::Optimal => {
            let device_buffer = settings.optimal.device_buffer;
            settings.optimal.buffer = raise_buffer_for_device(buffer, device_buffer)
                .ok_or(EditRejected::BuffersNotAllowed(ValueBlocker::DeviceBufferTooLarge { max_ms: device_buffer.ms() }))?;
            Ok(())
        }
        ModeKind::Strict => {
            let device_buffer = settings.strict.device_buffer;
            settings.strict.buffer = raise_buffer_for_device(buffer, device_buffer)
                .ok_or(EditRejected::BuffersNotAllowed(ValueBlocker::DeviceBufferTooLarge { max_ms: device_buffer.ms() }))?;
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

/// Правка переключателя «ЦАП понимает DoP» — только при выбранном устройстве
/// режима; значение-умолчание не хранится отдельной записью `settings.devices`
/// (ADR-25, ТЗ-145).
fn apply_device_dop(settings: &mut ModeSettings, mode: ModeKind, value: EditValue) -> Result<(), EditRejected> {
    let EditValue::Toggle(on) = value else { return Err(EditRejected::BadValue) };
    let device = match mode {
        ModeKind::Optimal => settings.optimal.device.clone(),
        ModeKind::Strict => settings.strict.device.clone(),
        ModeKind::Compatible => return Err(EditRejected::NotEditable),
    };
    let id = device.ok_or(EditRejected::NotEditable)?;
    let mut opts = settings.device_opts(&id);
    opts.dop = on;
    if opts == DeviceOpts::default() {
        settings.devices.remove(&id);
    } else {
        settings.devices.insert(id, opts);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use slint::Model as _;

    #[test]
    fn fixed_by_mode_row_is_disabled_with_note() {
        let settings = ModeSettings::default();
        let rows = build_rows(ModeKind::Compatible, &settings, &settings);
        let access = rows.iter().find(|r| r.param == ParamId::Access).expect("Access row must exist");
        assert!(!access.enabled);
        assert!(access.note.contains("задано режимом"));
    }

    /// ТЗ-131, §6.27, решение пользователя 2026-10-24: пункты списка «Буфер
    /// устройства» не зависят от текущего чернового буфера плеера — пара
    /// больше не отказывает, несовпадение разрешается автоподъёмом.
    #[test]
    fn device_buffer_options_all_enabled_regardless_of_draft_buffer() {
        let mut settings = ModeSettings::default();
        settings.optimal.buffer = BufferMsForTest::ms100();
        let rows = build_rows(ModeKind::Optimal, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::DeviceBuffer).expect("DeviceBuffer row must exist");
        let RowKind::List { options, .. } = &row.kind else { panic!("expected List") };
        assert!(options.iter().all(|o| o.enabled), "все пункты должны быть активны: {options:?}");
    }

    #[test]
    fn not_implemented_src_filter_option_is_disabled() {
        let settings = ModeSettings::default();
        let rows = build_rows(ModeKind::Compatible, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::SrcFilter).expect("SrcFilter row must exist");
        let RowKind::List { options, .. } = &row.kind else { panic!("expected List") };
        let short_latency = options
            .iter()
            .find(|o| o.label == i18n::SRC_FILTER_STEEP_SHORT_LATENCY)
            .expect("short-latency option must exist");
        assert!(!short_latency.enabled);
        assert_eq!(short_latency.hint, i18n::MODE_DIALOG_NOT_IMPLEMENTED);
    }

    #[test]
    fn editable_row_reflects_current_setting_value() {
        let mut settings = ModeSettings::default();
        settings.compatible.src_filter = SrcFilter::VerySlow;
        let rows = build_rows(ModeKind::Compatible, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::SrcFilter).expect("SrcFilter row must exist");
        let RowKind::List { selected, .. } = &row.kind else { panic!("expected List") };
        assert_eq!(*selected, 2);
    }

    #[test]
    fn strict_and_compatible_differ_in_availability_for_src_filter() {
        let settings = ModeSettings::default();
        let compatible_row = build_rows(ModeKind::Compatible, &settings, &settings)
            .into_iter()
            .find(|r| r.param == ParamId::SrcFilter)
            .expect("SrcFilter row must exist");
        let strict_row = build_rows(ModeKind::Strict, &settings, &settings)
            .into_iter()
            .find(|r| r.param == ParamId::SrcFilter)
            .expect("SrcFilter row must exist");
        assert!(compatible_row.enabled);
        assert!(!strict_row.enabled);
    }

    /// Вспомогательный конструктор буфера плеера для теста: 100 мс попадают
    /// в допустимый диапазон `BufferMs`, но ниже `buffer_floor_ms(Ms400)`
    /// (800 мс) — стартовое значение для проверки автоподъёма/клампа.
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

        let rows = build_rows(ModeKind::Compatible, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::SrcFilter).expect("SrcFilter row must exist");
        let RowKind::List { selected, .. } = &row.kind else { panic!("expected List") };
        assert_eq!(*selected, 2);
    }

    /// ТЗ-131, §6.27, решение пользователя 2026-10-24: повышение «Буфера
    /// устройства» до значения с более высокой границей поднимает черновой
    /// буфер плеера ровно до этой границы вместо отказа.
    #[test]
    fn device_buffer_edit_raises_draft_buffer_to_floor() {
        let mut settings = ModeSettings::default();
        settings.optimal.buffer = BufferMsForTest::ms100();

        apply_edit(&mut settings, ModeKind::Optimal, ParamId::DeviceBuffer, EditValue::Index(4))
            .expect("Index(4) = Ms400 — автоподъём, допустимая правка");

        assert_eq!(settings.optimal.device_buffer, DeviceBuffer::Ms400);
        assert_eq!(settings.optimal.buffer.get(), 800);
    }

    /// ТЗ-131, §6.27: значение ползунка «Буфер декодирование → вывод» ниже
    /// границы текущего буфера устройства клампится до неё, а не
    /// отклоняется.
    #[test]
    fn buffer_slider_edit_below_floor_clamps_to_floor() {
        let mut settings = ModeSettings::default();
        settings.optimal.device_buffer = DeviceBuffer::Ms200; // floor = 400 мс

        apply_edit(&mut settings, ModeKind::Optimal, ParamId::Buffer, EditValue::Slider(100))
            .expect("значение ниже границы клампится, не отклоняется");

        assert_eq!(settings.optimal.buffer.get(), 400);
    }

    /// Решение пользователя 2026-10-24 (ТЗ-131): связь односторонняя — буфер
    /// плеера, уже поднятый автоподъёмом, последующее уменьшение буфера
    /// устройства назад не опускает.
    #[test]
    fn device_buffer_decrease_does_not_lower_already_raised_buffer() {
        let mut settings = ModeSettings::default();
        settings.optimal.buffer = BufferMsForTest::ms100();
        apply_edit(&mut settings, ModeKind::Optimal, ParamId::DeviceBuffer, EditValue::Index(4))
            .expect("Ms400 — допустимая правка");
        assert_eq!(settings.optimal.buffer.get(), 800);

        apply_edit(&mut settings, ModeKind::Optimal, ParamId::DeviceBuffer, EditValue::Index(0))
            .expect("Ms20 — допустимая правка");

        assert_eq!(settings.optimal.device_buffer, DeviceBuffer::Ms20);
        assert_eq!(settings.optimal.buffer.get(), 800);
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

    /// §6.18: `param_index`/`param_at` — взаимно обратное соответствие по
    /// позиции в `PARAMS` для любого параметра.
    #[test]
    fn param_index_and_param_at_roundtrip() {
        let index = param_index(ParamId::DeviceBuffer);
        assert!(index >= 0);
        assert_eq!(param_at(index), Some(ParamId::DeviceBuffer));
        assert_eq!(param_at(-1), None);
    }

    /// §6.27, ТЗ-94, ТЗ-130: отключённый пункт списка несёт подсказку прямо
    /// в подписи строки панели — у `ModeParamRow.options` нет отдельного
    /// поля доступности пункта.
    #[test]
    fn to_param_row_appends_hint_to_disabled_option_label() {
        let settings = ModeSettings::default();
        let rows = build_rows(ModeKind::Compatible, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::SrcFilter).expect("SrcFilter row must exist");
        let param_row = to_param_row(row);

        assert_eq!(param_row.kind, 0);
        let short_latency = param_row
            .options
            .iter()
            .find(|o| o.starts_with(i18n::SRC_FILTER_STEEP_SHORT_LATENCY))
            .expect("short-latency option must exist");
        assert!(short_latency.contains(i18n::MODE_DIALOG_NOT_IMPLEMENTED));
    }

    /// ТЗ-131, §6.27: буфер устройства `Ms100` поднимает нижнюю границу
    /// ползунка «Буфер декодирование → вывод» до 200 мс, строка границы
    /// всегда видна и называет буфер устройства, поднявший её.
    #[test]
    fn row_buffer_floor_raised_by_device_buffer_shows_boundary_note() {
        let mut settings = ModeSettings::default();
        settings.optimal.device_buffer = DeviceBuffer::Ms100;
        let rows = build_rows(ModeKind::Optimal, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::Buffer).expect("Buffer row must exist");
        assert_eq!(row.kind, RowKind::Slider { value: row_value(row), min: 200, max: 10_000 });
        assert_eq!(row.note, "минимум 200 мс — вдвое больше буфера устройства (100 мс)");
    }

    /// ТЗ-131, §6.27: когда граница равна абсолютному минимуму (`Ms20`/
    /// `Ms40`, 100 мс), строка границы всё равно видна постоянно, но без
    /// упоминания буфера устройства.
    #[test]
    fn row_buffer_floor_at_absolute_minimum_shows_plain_boundary_note() {
        let settings = ModeSettings::default(); // optimal.device_buffer == Ms40 → floor 100
        let rows = build_rows(ModeKind::Optimal, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::Buffer).expect("Buffer row must exist");
        assert_eq!(row.note, "минимум 100 мс");
    }

    /// ТЗ-131, §6.27: в Совместимом режиме `DeviceBuffer` не применяется —
    /// нижняя граница ползунка всегда 100 мс без строки границы.
    #[test]
    fn row_buffer_floor_is_100_in_compatible_mode() {
        let settings = ModeSettings::default();
        let rows = build_rows(ModeKind::Compatible, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::Buffer).expect("Buffer row must exist");
        assert_eq!(row.kind, RowKind::Slider { value: row_value(row), min: 100, max: 10_000 });
        assert!(row.note.is_empty());
    }

    /// Текущее значение ползунка `Buffer` для теста выше (значение не
    /// меняется правкой границы — ТЗ-131).
    fn row_value(row: &DialogRow) -> u16 {
        let RowKind::Slider { value, .. } = &row.kind else { panic!("expected Slider") };
        *value
    }

    /// ТЗ-131, §6.27, AM1.0-B35 (step 7): последняя правка списка «Буфер
    /// устройства» подняла черновой буфер плеера до границы — строка
    /// «Поднят автоматически до N мс» видна.
    #[test]
    fn row_buffer_note_shows_auto_raise_line_after_device_buffer_lift() {
        let mut saved = ModeSettings::default();
        saved.optimal.buffer = BufferMsForTest::ms100();
        saved.optimal.device_buffer = DeviceBuffer::Ms20;
        let mut draft = saved.clone();

        apply_edit(&mut draft, ModeKind::Optimal, ParamId::DeviceBuffer, EditValue::Index(3))
            .expect("Ms200 — допустимая правка");
        assert_eq!(draft.optimal.buffer.get(), 400);

        let rows = build_rows(ModeKind::Optimal, &draft, &saved);
        let row = rows.iter().find(|r| r.param == ParamId::Buffer).expect("Buffer row must exist");
        assert_eq!(row.note, "минимум 400 мс — вдвое больше буфера устройства (200 мс)\nПоднят автоматически до 400 мс");
    }

    /// ТЗ-131, §6.27, AM1.0-B35 (step 7): после «Сохранить» черновик и
    /// сохранённые настройки совпадают — автоподъём уже применён, строка
    /// не повторяется (устройство в draft/saved не отличается).
    #[test]
    fn row_buffer_note_hides_auto_raise_line_once_saved() {
        let mut settings = ModeSettings::default();
        settings.optimal.buffer = BufferMsForTest::ms100();
        settings.optimal.device_buffer = DeviceBuffer::Ms20;
        apply_edit(&mut settings, ModeKind::Optimal, ParamId::DeviceBuffer, EditValue::Index(3))
            .expect("Ms200 — допустимая правка");
        assert_eq!(settings.optimal.buffer.get(), 400);

        let rows = build_rows(ModeKind::Optimal, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::Buffer).expect("Buffer row must exist");
        assert!(!row.note.contains("Поднят автоматически"), "note: {}", row.note);
    }

    /// ТЗ-131, §6.27, AM1.0-B35 (step 7): пользователь сам поднимает буфер
    /// плеера выше границы правкой ползунка — текущее значение больше не
    /// равно границе, строка автоподъёма не показывается.
    #[test]
    fn row_buffer_note_not_shown_when_user_raises_above_floor_manually() {
        let mut saved = ModeSettings::default();
        saved.optimal.buffer = BufferMsForTest::ms100();
        saved.optimal.device_buffer = DeviceBuffer::Ms20;
        let mut draft = saved.clone();

        apply_edit(&mut draft, ModeKind::Optimal, ParamId::DeviceBuffer, EditValue::Index(3))
            .expect("Ms200 — допустимая правка");
        assert_eq!(draft.optimal.buffer.get(), 400);
        apply_edit(&mut draft, ModeKind::Optimal, ParamId::Buffer, EditValue::Slider(600))
            .expect("600 мс выше границы — допустимая правка");

        let rows = build_rows(ModeKind::Optimal, &draft, &saved);
        let row = rows.iter().find(|r| r.param == ParamId::Buffer).expect("Buffer row must exist");
        assert!(!row.note.contains("Поднят автоматически"), "note: {}", row.note);
    }

    /// §6.27: `RowKind::Slider` → `kind == 2` с значением/границами, перенесёнными
    /// без потери точности (`f32::from(u16)`, без `as`).
    #[test]
    fn to_param_row_maps_slider_kind_and_bounds() {
        let settings = ModeSettings::default();
        let rows = build_rows(ModeKind::Compatible, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::Buffer).expect("Buffer row must exist");
        let param_row = to_param_row(row);

        assert_eq!(param_row.kind, 2);
        assert_eq!(param_row.min, 100.0);
        assert_eq!(param_row.max, 10_000.0);
    }

    /// ADR-25, ТЗ-145: с выбранным устройством и без записи в `devices`
    /// переключатель «ЦАП понимает DoP» показывает умолчание (true) и активен.
    #[test]
    fn device_dop_row_reflects_default_when_no_entry() {
        let Ok(device) = "hw:CARD=A,DEV=0".parse::<music_player_rs::audio::backend::HwDeviceId>() else {
            panic!("valid HwDeviceId")
        };
        let mut settings = ModeSettings::default();
        settings.optimal.device = Some(device);
        let rows = build_rows(ModeKind::Optimal, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::DeviceDop).expect("DeviceDop row must exist");

        assert!(row.enabled);
        assert_eq!(row.kind, RowKind::Toggle { on: true });
    }

    /// ADR-25, ТЗ-145: запись `dop: false` для выбранного устройства
    /// отражается в строке диалога.
    #[test]
    fn device_dop_row_reflects_stored_false() {
        let Ok(device) = "hw:CARD=A,DEV=0".parse::<music_player_rs::audio::backend::HwDeviceId>() else {
            panic!("valid HwDeviceId")
        };
        let mut settings = ModeSettings::default();
        settings.strict.device = Some(device.clone());
        settings.devices.insert(device, DeviceOpts { dop: false });
        let rows = build_rows(ModeKind::Strict, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::DeviceDop).expect("DeviceDop row must exist");

        assert!(row.enabled);
        assert_eq!(row.kind, RowKind::Toggle { on: false });
    }

    /// ADR-25, ТЗ-145: без выбранного устройства переключатель неактивен с
    /// пояснением.
    #[test]
    fn device_dop_row_disabled_without_device() {
        let settings = ModeSettings::default();
        let rows = build_rows(ModeKind::Optimal, &settings, &settings);
        let row = rows.iter().find(|r| r.param == ParamId::DeviceDop).expect("DeviceDop row must exist");

        assert!(!row.enabled);
        assert_eq!(row.note, i18n::MODE_DIALOG_DEVICE_NOT_SELECTED);
    }

    /// ADR-25, ТЗ-145: правка переключателя пишет/удаляет запись
    /// `settings.devices` для устройства режима, умолчание не хранится.
    #[test]
    fn device_dop_edit_writes_and_clears_default_entry() {
        let Ok(device) = "hw:CARD=A,DEV=0".parse::<music_player_rs::audio::backend::HwDeviceId>() else {
            panic!("valid HwDeviceId")
        };
        let mut settings = ModeSettings::default();
        settings.optimal.device = Some(device.clone());

        apply_edit(&mut settings, ModeKind::Optimal, ParamId::DeviceDop, EditValue::Toggle(false))
            .expect("Toggle(false) — допустимая правка");
        assert_eq!(settings.devices.get(&device), Some(&DeviceOpts { dop: false }));

        apply_edit(&mut settings, ModeKind::Optimal, ParamId::DeviceDop, EditValue::Toggle(true))
            .expect("Toggle(true) — допустимая правка");
        assert!(!settings.devices.contains_key(&device));
    }

    /// ADR-25, ТЗ-145: без выбранного устройства и в Совместимом режиме
    /// правка отклоняется, `settings` не меняется.
    #[test]
    fn device_dop_edit_rejected_without_device_or_in_compatible() {
        let mut settings = ModeSettings::default();
        let before = settings.clone();

        let result = apply_edit(&mut settings, ModeKind::Optimal, ParamId::DeviceDop, EditValue::Toggle(false));
        assert_eq!(result, Err(EditRejected::NotEditable));
        assert_eq!(settings, before);

        let result = apply_edit(&mut settings, ModeKind::Compatible, ParamId::DeviceDop, EditValue::Toggle(false));
        assert_eq!(result, Err(EditRejected::NotEditable));
        assert_eq!(settings, before);
    }
}
