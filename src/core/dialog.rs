//! Модель диалога настроек без Slint (ADR-11, ADR-19, §2.10): черновик,
//! отличия черновика от действующих значений и недопустимые значения
//! (ТЗ-25, ТЗ-27, ТЗ-29, ТЗ-30).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::audio::visualizer::{VisualizationMode, VizSettings};
use crate::persist::settings_file::{ColumnDef, SaveInterval, Settings};
use crate::persist::state_file::SessionState;
use crate::settings::ColumnId;
use crate::settings::params::{ModeSettingsDiff, ModeSettingsUpdate};
use crate::settings::playback::{
    buffers_allowed, CompatibleOpts, ModeKind, ModeSettings, OptimalOpts, StrictOpts, ValueBlocker,
};
use crate::theme::ThemeData;

/// Черновик диалога (§2.10, ADR-11, ТЗ-25): только параметры диалога.
/// Создаётся при открытии из действующих значений; состояние сессии в него
/// не входит по построению. «Отмена» / Esc уничтожают черновик — ничего не
/// применяется и не записывается (ТЗ-26).
#[derive(Clone, PartialEq, Debug)]
pub struct DialogDraft {
    /// Настройки окна настроек — содержимое `settings.toml` (ТЗ-25, §2.4).
    pub settings: Settings,
    /// Тип визуализации (Т-1): сравнивается с действующим состоянием сессии.
    pub viz_mode: VisualizationMode,
    /// «Сбросить ширины» нажата (Т-2).
    pub reset_widths: bool,
    /// Режим, параметры которого показаны (ТЗ-30): не сравнивается и не
    /// сохраняется.
    pub shown_mode: ModeKind,
    /// Данные темы, прочитанные `apap-io` после выбора (ADR-20).
    pub theme: ThemeLoad,
}

/// Состояние чтения темы, выбранной в диалоге (§2.10, ADR-20, ТЗ-22).
/// `Loading` и `Invalid` — недопустимые значения: «Сохранить» не применяет
/// тему, данных которой ещё нет (ТЗ-27, ТЗ-29).
#[derive(Clone, PartialEq, Debug)]
pub enum ThemeLoad {
    /// Тема в диалоге не выбиралась — действует прежняя.
    Unchanged,
    /// Тема читается потоком `apap-io`; `gen` — поколение задания
    /// `IoJob::ReadTheme`, по которому сопоставляется ответ (ADR-20).
    Loading { gen: u64 },
    /// Тема прочитана; при «Сохранить» повторного чтения нет (ADR-11, ТЗ-29).
    Ready(Arc<ThemeData>),
    /// Файл темы с ошибкой; `reason` — причина из разбора темы (ТЗ-27).
    Invalid { reason: Box<str> },
}

/// Отличия черновика от действующих значений (§2.10, ADR-11, ТЗ-27, ТЗ-29):
/// поэлементное сравнение по значению, поэтому «изменить и вернуть» отличия
/// не даёт. По отличиям строится план применения по строкам §5.2 ТЗ.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SettingsDiff {
    /// Изменённые группы параметров окна настроек (ТЗ-29).
    pub fields: BTreeSet<SettingsField>,
    /// Изменённые параметры режимов; None — режимы не менялись.
    pub modes: Option<ModeSettingsDiff>,
    /// Новый тип визуализации (Т-1); None — не менялся.
    pub viz_mode: Option<VisualizationMode>,
    /// «Сбросить ширины» нажата (Т-2).
    pub reset_widths: bool,
}

impl SettingsDiff {
    /// Отличий нет (ТЗ-27): кнопка «Сохранить» неактивна, запись и
    /// применение не выполняются (ТЗ-28, ТЗ-29).
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.modes.is_none() && self.viz_mode.is_none() && !self.reset_widths
    }
}

/// Группа параметров окна настроек, применяемая одной строкой §5.2 ТЗ
/// (§2.10, ТЗ-29). Порядок вариантов задаёт порядок обхода `BTreeSet`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SettingsField {
    Theme,
    SaveInterval,
    MinimizeToTray,
    ScrollToPlaying,
    TopPanel,
    ColumnVisibility,
    ColumnOrder,
    ColumnDefs,
    Covers,
    InfoLabels,
    Visualization,
    VizCacheBudget,
}

/// Недопустимое значение (§2.10, ТЗ-27): параметр и причина для подсказки
/// «Сохранить».
#[derive(Clone, PartialEq, Debug)]
pub struct Invalid {
    pub param: &'static str,
    pub reason: Box<str>,
}

/// Отличия черновика от действующих значений (§2.10, §6.9, ТЗ-27, ТЗ-29,
/// ТЗ-30). Базой сравнения служат действующие настройки и действующее
/// состояние сессии, а не снимок при открытии (ADR-11): пока диалог открыт,
/// настройки меняются только им. `draft.shown_mode` не читается (ТЗ-30).
pub fn diff(cur: &Settings, state: &SessionState, draft: &DialogDraft) -> SettingsDiff {
    // Без `..` — новое поле `Settings` без строки здесь не компилируется.
    let Settings {
        theme: a_theme,
        save_interval: a_save_interval,
        minimize_to_tray: a_minimize_to_tray,
        scroll_to_playing: a_scroll_to_playing,
        top_panel: a_top_panel,
        columns: a_columns,
        covers: a_covers,
        info_labels: a_info_labels,
        visualization: a_visualization,
        playback: a_playback,
    } = cur;
    let Settings {
        theme: b_theme,
        save_interval: b_save_interval,
        minimize_to_tray: b_minimize_to_tray,
        scroll_to_playing: b_scroll_to_playing,
        top_panel: b_top_panel,
        columns: b_columns,
        covers: b_covers,
        info_labels: b_info_labels,
        visualization: b_visualization,
        playback: b_playback,
    } = &draft.settings;

    let (column_visibility, column_defs) = column_defs_differ(&a_columns.defs, &b_columns.defs);
    let (visualization, viz_cache_budget) = visualization_differs(a_visualization, b_visualization);

    // Тема сравнивается по имени, как и остальные поля (§6.9): выбрать другую
    // тему и вернуть прежнюю — отличия нет (ТЗ-27). Состояние чтения
    // `draft.theme` — предмет `validate`, не сравнения (§2.10).
    let groups = [
        (SettingsField::Theme, a_theme != b_theme),
        (SettingsField::SaveInterval, a_save_interval != b_save_interval),
        (SettingsField::MinimizeToTray, a_minimize_to_tray != b_minimize_to_tray),
        (SettingsField::ScrollToPlaying, a_scroll_to_playing != b_scroll_to_playing),
        (SettingsField::TopPanel, a_top_panel != b_top_panel),
        (SettingsField::ColumnVisibility, column_visibility),
        (SettingsField::ColumnOrder, a_columns.order != b_columns.order),
        (SettingsField::ColumnDefs, column_defs),
        (SettingsField::Covers, a_covers != b_covers),
        (SettingsField::InfoLabels, a_info_labels != b_info_labels),
        (SettingsField::Visualization, visualization),
        (SettingsField::VizCacheBudget, viz_cache_budget),
    ];
    let fields = groups.into_iter().filter(|(_, differs)| *differs).map(|(field, _)| field).collect();

    // Пустое множество параметров режимов не отправляется движку (ТЗ-29).
    let modes = Some(a_playback.diff(b_playback)).filter(|d| !d.changed.is_empty());
    let viz_mode = (draft.viz_mode != state.viz_mode()).then_some(draft.viz_mode);
    // Сброс пустого набора ширин отличием не является (§6.9, Т-2).
    let reset_widths = draft.reset_widths && !state.column_widths().is_empty();

    SettingsDiff { fields, modes, viz_mode, reset_widths }
}

/// Перебазирование черновика при изменении действующих настроек в обход
/// диалога (SP1.0-B13, вариант А, ТЗ-25, ТЗ-27, ADR-11): пока диалог открыт,
/// единственный известный случай — автовыбор устройства Оптимального/
/// Строгого режима (`save_auto_selected_device`, ТЗ-143). Для каждого
/// значения, которое пользователь не редактировал в черновике (оно равно
/// прежнему действующему `old`), черновик получает новое действующее
/// значение `new`; значение, которое пользователь уже изменил в черновике,
/// остаётся как есть — внешнее изменение его не перетирает. Для `playback`
/// сравнение идёт по каждому параметру режима отдельно (`rebase_compatible`/
/// `rebase_optimal`/`rebase_strict`), а не по режиму целиком: правка одного
/// параметра режима в черновике не мешает перебазированию устройства того
/// же режима. Чистая функция, без записи и без обращения к `AppCore`.
///
/// Вызывается только из `AppCore::save_settings_now` на пути, отличном от
/// «Сохранить» диалога (`dialog_save` забирает черновик до вызова
/// `save_settings_now`, поэтому там черновика уже нет и `rebase` не
/// затрагивает её семантику).
pub fn rebase(draft: &mut DialogDraft, old: &Settings, new: &Settings) {
    // Без `..` — новое поле `Settings` без строки здесь не компилируется
    // (как в `diff`).
    let Settings {
        theme: old_theme,
        save_interval: old_save_interval,
        minimize_to_tray: old_minimize_to_tray,
        scroll_to_playing: old_scroll_to_playing,
        top_panel: old_top_panel,
        columns: old_columns,
        covers: old_covers,
        info_labels: old_info_labels,
        visualization: old_visualization,
        playback: old_playback,
    } = old;
    let Settings {
        theme: new_theme,
        save_interval: new_save_interval,
        minimize_to_tray: new_minimize_to_tray,
        scroll_to_playing: new_scroll_to_playing,
        top_panel: new_top_panel,
        columns: new_columns,
        covers: new_covers,
        info_labels: new_info_labels,
        visualization: new_visualization,
        playback: new_playback,
    } = new;

    rebase_field(&mut draft.settings.theme, old_theme, new_theme);
    rebase_field(&mut draft.settings.save_interval, old_save_interval, new_save_interval);
    rebase_field(&mut draft.settings.minimize_to_tray, old_minimize_to_tray, new_minimize_to_tray);
    rebase_field(&mut draft.settings.scroll_to_playing, old_scroll_to_playing, new_scroll_to_playing);
    rebase_field(&mut draft.settings.top_panel, old_top_panel, new_top_panel);
    rebase_field(&mut draft.settings.columns, old_columns, new_columns);
    rebase_field(&mut draft.settings.covers, old_covers, new_covers);
    rebase_field(&mut draft.settings.info_labels, old_info_labels, new_info_labels);
    rebase_field(&mut draft.settings.visualization, old_visualization, new_visualization);
    rebase_playback(&mut draft.settings.playback, old_playback, new_playback);
}

/// Одно значение черновика (§2.10, ADR-11): если пользователь его не
/// редактировал (оно равно прежнему действующему `old`), оно следует новому
/// действующему значению `new`; иначе остаётся как в черновике.
fn rebase_field<T: Clone + PartialEq>(draft: &mut T, old: &T, new: &T) {
    if draft == old {
        draft.clone_from(new);
    }
}

/// Перебазирование режимов (ТЗ-143, ADR-24, ADR-25) по каждому параметру
/// отдельно — автовыбор устройства Оптимального/Строгого режима следует
/// новому значению, даже если пользователь уже редактирует другой параметр
/// того же режима в черновике. `devices` (ТЗ-145, ADR-25) не меняется
/// `save_auto_selected_device`, но перебазируется как единое целое — так же,
/// как прочие групповые поля `Settings`.
fn rebase_playback(draft: &mut ModeSettings, old: &ModeSettings, new: &ModeSettings) {
    // Без `..` — новый режим/поле `ModeSettings` без строки здесь не
    // компилируется.
    let ModeSettings { compatible: old_compatible, optimal: old_optimal, strict: old_strict, devices: old_devices } =
        old;
    let ModeSettings { compatible: new_compatible, optimal: new_optimal, strict: new_strict, devices: new_devices } =
        new;

    rebase_compatible(&mut draft.compatible, old_compatible, new_compatible);
    rebase_optimal(&mut draft.optimal, old_optimal, new_optimal);
    rebase_strict(&mut draft.strict, old_strict, new_strict);
    rebase_field(&mut draft.devices, old_devices, new_devices);
}

/// Перебазирование Совместимого режима по параметрам (ТЗ-93, §2.2).
fn rebase_compatible(draft: &mut CompatibleOpts, old: &CompatibleOpts, new: &CompatibleOpts) {
    // Без `..` — новое поле `CompatibleOpts` без строки здесь не компилируется.
    let CompatibleOpts {
        device: old_device,
        fixed_rate: old_fixed_rate,
        src_filter: old_src_filter,
        dither: old_dither,
        dsd_filter: old_dsd_filter,
        dsd_gain_comp: old_dsd_gain_comp,
        buffer: old_buffer,
    } = old;
    let CompatibleOpts {
        device: new_device,
        fixed_rate: new_fixed_rate,
        src_filter: new_src_filter,
        dither: new_dither,
        dsd_filter: new_dsd_filter,
        dsd_gain_comp: new_dsd_gain_comp,
        buffer: new_buffer,
    } = new;
    rebase_field(&mut draft.device, old_device, new_device);
    rebase_field(&mut draft.fixed_rate, old_fixed_rate, new_fixed_rate);
    rebase_field(&mut draft.src_filter, old_src_filter, new_src_filter);
    rebase_field(&mut draft.dither, old_dither, new_dither);
    rebase_field(&mut draft.dsd_filter, old_dsd_filter, new_dsd_filter);
    rebase_field(&mut draft.dsd_gain_comp, old_dsd_gain_comp, new_dsd_gain_comp);
    rebase_field(&mut draft.buffer, old_buffer, new_buffer);
}

/// Перебазирование Оптимального режима по параметрам (ТЗ-143, §2.2): `device`
/// — тот самый автовыбор `save_auto_selected_device`, остальные параметры
/// независимы от него.
fn rebase_optimal(draft: &mut OptimalOpts, old: &OptimalOpts, new: &OptimalOpts) {
    // Без `..` — новое поле `OptimalOpts` без строки здесь не компилируется.
    let OptimalOpts {
        device: old_device,
        rate_fallback: old_rate_fallback,
        src_filter: old_src_filter,
        dither: old_dither,
        volume_lock: old_volume_lock,
        dsd_above_dac: old_dsd_above_dac,
        dsd_filter: old_dsd_filter,
        dsd_gain_comp: old_dsd_gain_comp,
        buffer: old_buffer,
        device_buffer: old_device_buffer,
    } = old;
    let OptimalOpts {
        device: new_device,
        rate_fallback: new_rate_fallback,
        src_filter: new_src_filter,
        dither: new_dither,
        volume_lock: new_volume_lock,
        dsd_above_dac: new_dsd_above_dac,
        dsd_filter: new_dsd_filter,
        dsd_gain_comp: new_dsd_gain_comp,
        buffer: new_buffer,
        device_buffer: new_device_buffer,
    } = new;
    rebase_field(&mut draft.device, old_device, new_device);
    rebase_field(&mut draft.rate_fallback, old_rate_fallback, new_rate_fallback);
    rebase_field(&mut draft.src_filter, old_src_filter, new_src_filter);
    rebase_field(&mut draft.dither, old_dither, new_dither);
    rebase_field(&mut draft.volume_lock, old_volume_lock, new_volume_lock);
    rebase_field(&mut draft.dsd_above_dac, old_dsd_above_dac, new_dsd_above_dac);
    rebase_field(&mut draft.dsd_filter, old_dsd_filter, new_dsd_filter);
    rebase_field(&mut draft.dsd_gain_comp, old_dsd_gain_comp, new_dsd_gain_comp);
    rebase_field(&mut draft.buffer, old_buffer, new_buffer);
    rebase_field(&mut draft.device_buffer, old_device_buffer, new_device_buffer);
}

/// Перебазирование Строгого режима по параметрам (ТЗ-143, §2.2): `device` —
/// тот самый автовыбор `save_auto_selected_device`.
fn rebase_strict(draft: &mut StrictOpts, old: &StrictOpts, new: &StrictOpts) {
    // Без `..` — новое поле `StrictOpts` без строки здесь не компилируется.
    let StrictOpts { device: old_device, buffer: old_buffer, device_buffer: old_device_buffer } = old;
    let StrictOpts { device: new_device, buffer: new_buffer, device_buffer: new_device_buffer } = new;
    rebase_field(&mut draft.device, old_device, new_device);
    rebase_field(&mut draft.buffer, old_buffer, new_buffer);
    rebase_field(&mut draft.device_buffer, old_device_buffer, new_device_buffer);
}

/// Подпись темы в подсказке «Сохранить» (§6.9, ТЗ-27).
const PARAM_THEME: &str = "Тема";
/// Подписи буфера устройства по режимам: подпись параметра из диалога режима
/// и режим, в котором сочетание недопустимо (§6.9, ТЗ-27, ТЗ-131).
const PARAM_DEVICE_BUFFER_OPTIMAL: &str = "Буфер устройства (Оптимальный режим)";
const PARAM_DEVICE_BUFFER_STRICT: &str = "Буфер устройства (Строгий режим)";

/// Недопустимые значения черновика (§2.10, §6.9, ТЗ-27): пока список не пуст,
/// «Сохранить» неактивна, а подсказка — первый элемент в виде
/// «<param>: <reason>». Порядок: тема, затем режимы в порядке объявления.
/// Допустимые множества §2.4 обеспечены типами `Settings`; проверяются
/// состояние чтения темы (ТЗ-29) и И-Р22 `buffers_allowed` для пар «буфер
/// плеера / буфер устройства» Оптимального и Строгого режимов.
pub fn validate(draft: &DialogDraft) -> Vec<Invalid> {
    let mut invalid = Vec::new();

    match &draft.theme {
        ThemeLoad::Unchanged | ThemeLoad::Ready(_) => {}
        ThemeLoad::Loading { .. } => {
            invalid.push(Invalid { param: PARAM_THEME, reason: "тема загружается".into() });
        }
        ThemeLoad::Invalid { reason } => {
            invalid.push(Invalid { param: PARAM_THEME, reason: reason.clone() });
        }
    }

    // Без `..` — новый режим в `ModeSettings` без строки здесь не компилируется.
    // У Совместимого режима буфера устройства нет (ТЗ-131).
    let ModeSettings { compatible: _, optimal, strict, devices: _ } = &draft.settings.playback;
    let pairs = [
        (PARAM_DEVICE_BUFFER_OPTIMAL, optimal.buffer, optimal.device_buffer),
        (PARAM_DEVICE_BUFFER_STRICT, strict.buffer, strict.device_buffer),
    ];
    for (param, buffer, device_buffer) in pairs {
        if let Err(blocker) = buffers_allowed(buffer, device_buffer) {
            invalid.push(Invalid { param, reason: blocker_reason(blocker) });
        }
    }

    invalid
}

/// Причина недопустимого сочетания буферов (И-Р22, ТЗ-27, ТЗ-131):
/// формулировки вариантов `ValueBlocker`, те же, что в диалоге режима.
fn blocker_reason(blocker: ValueBlocker) -> Box<str> {
    match blocker {
        ValueBlocker::PlayerBufferTooSmall { need_ms } => format!("увеличьте буфер плеера до {need_ms} мс"),
        ValueBlocker::DeviceBufferTooLarge { max_ms } => format!("уменьшите буфер устройства до {max_ms} мс"),
    }
    .into_boxed_str()
}

/// Шаг применения по строкам §5.2 ТЗ (§2.10, §6.9, ТЗ-29): одно действие
/// «Сохранить» над одной группой параметров. Шаги строит [`apply_plan`] только
/// для отличий; порядок вариантов — порядок шагов в плане.
#[derive(Clone, PartialEq, Debug)]
pub enum ApplyStep {
    /// Цветовая тема — сразу в UI, данными, прочитанными при выборе в диалоге;
    /// повторного чтения файла нет (ADR-11, ADR-20, ТЗ-29).
    Theme(Arc<ThemeData>),
    /// Набор (видимость), порядок и описания колонок — сразу в UI;
    /// пересобирается только затронутое, скрытие отсортированной колонки не
    /// меняет ключ и видимый порядок (ТЗ-31).
    Columns { visibility: bool, order: bool, defs: bool },
    /// Размеры верхней панели, включая размер обложки, — сразу в UI.
    TopPanel,
    /// Приоритет источников, имена файлов, обложки из интернета — сразу в UI
    /// и воркере обложек (новый `CoverConfig`).
    Covers,
    /// Подписи информации о треке (прочие не-звуковые параметры) — сразу в UI.
    InfoLabels,
    /// Цвета и параметры визуализации, её включение/выключение — сразу в UI.
    Visualization,
    /// Параметры кэша визуализации (в памяти, на диске) — сразу.
    VizCacheBudget,
    /// Сворачивание в трей — сразу (только Linux, В-1).
    Tray,
    /// Прокрутка к играющему треку (прочие не-звуковые параметры) — сразу.
    ScrollToPlaying,
    /// Интервал N — сразу: `tracker.set_interval(n, now)` (У-1, ТЗ-33).
    SaveInterval(SaveInterval),
    /// Тип визуализации — сразу в UI; действующее состояние с `Origin::User`
    /// → `state.toml` (Т-1).
    VizMode(VisualizationMode),
    /// «Сбросить ширины» — сразу в UI; пустой набор ширин в действующее
    /// состояние с `Origin::User` → `state.toml` (Т-2).
    ResetWidths,
    /// Опции режимов — `EngineCmd::SetModeSettings`; способ применения в
    /// тракте выбирает движок по `ApplyKind` (ТЗ-137 (01_audio_modes)).
    Engine(ModeSettingsUpdate),
}

/// План применения отличий (§2.10, §6.9, ТЗ-29, §5.2 ТЗ): шаг строится только
/// для того, что есть в `diff`; пустые отличия дают пустой план. Чистая
/// функция. Порядок шагов — порядок объявления вариантов [`ApplyStep`], он же
/// порядок §6.9: UI, затем состояние сессии, последним — движок.
///
/// Шаг темы строится по `diff.fields`, а не по `draft.theme`: тема, выбранная
/// и возвращённая к прежней, не применяется. Если тема в отличиях, а её данных
/// нет (`validate` такой черновик отсекает), шаг темы пропускается.
pub fn apply_plan(diff: &SettingsDiff, draft: &DialogDraft) -> Vec<ApplyStep> {
    let has = |field: SettingsField| diff.fields.contains(&field);
    let mut plan = Vec::new();

    if has(SettingsField::Theme) {
        if let ThemeLoad::Ready(data) = &draft.theme {
            plan.push(ApplyStep::Theme(Arc::clone(data)));
        }
    }

    let visibility = has(SettingsField::ColumnVisibility);
    let order = has(SettingsField::ColumnOrder);
    let defs = has(SettingsField::ColumnDefs);
    if visibility || order || defs {
        plan.push(ApplyStep::Columns { visibility, order, defs });
    }

    let simple = [
        (SettingsField::TopPanel, ApplyStep::TopPanel),
        (SettingsField::Covers, ApplyStep::Covers),
        (SettingsField::InfoLabels, ApplyStep::InfoLabels),
        (SettingsField::Visualization, ApplyStep::Visualization),
        (SettingsField::VizCacheBudget, ApplyStep::VizCacheBudget),
        (SettingsField::MinimizeToTray, ApplyStep::Tray),
        (SettingsField::ScrollToPlaying, ApplyStep::ScrollToPlaying),
        (SettingsField::SaveInterval, ApplyStep::SaveInterval(draft.settings.save_interval)),
    ];
    plan.extend(simple.into_iter().filter(|(field, _)| has(*field)).map(|(_, step)| step));

    if let Some(mode) = diff.viz_mode {
        plan.push(ApplyStep::VizMode(mode));
    }
    if diff.reset_widths {
        plan.push(ApplyStep::ResetWidths);
    }
    // Команда движку — только если изменился параметр режима (ТЗ-29).
    if let Some(modes) = &diff.modes {
        plan.push(ApplyStep::Engine(ModeSettingsUpdate {
            settings: draft.settings.playback.clone(),
            diff: Some(modes.clone()),
        }));
    }

    plan
}

/// Отличия описаний колонок (§2.10, ТЗ-29, ТЗ-31): `(видимость, прочее)`.
/// Видимость — отдельная группа: скрытие колонки пересобирает только модель
/// колонок. Несовпадение набора идентификаторов относится к описаниям.
fn column_defs_differ(a: &BTreeMap<ColumnId, ColumnDef>, b: &BTreeMap<ColumnId, ColumnDef>) -> (bool, bool) {
    let mut visibility = false;
    let mut defs = a.len() != b.len();
    for ((a_id, a_def), (b_id, b_def)) in a.iter().zip(b.iter()) {
        if a_id != b_id {
            defs = true;
            continue;
        }
        // Без `..` — новое поле `ColumnDef` без строки здесь не компилируется.
        let ColumnDef {
            title: a_title,
            priority: a_priority,
            min_width: a_min_width,
            max_width: a_max_width,
            max_width_percent: a_max_width_percent,
            visible: a_visible,
            kind: a_kind,
        } = a_def;
        let ColumnDef {
            title: b_title,
            priority: b_priority,
            min_width: b_min_width,
            max_width: b_max_width,
            max_width_percent: b_max_width_percent,
            visible: b_visible,
            kind: b_kind,
        } = b_def;
        visibility |= a_visible != b_visible;
        defs |= a_title != b_title
            || a_priority != b_priority
            || a_min_width != b_min_width
            || a_max_width != b_max_width
            || a_max_width_percent != b_max_width_percent
            || a_kind != b_kind;
    }
    (visibility, defs)
}

/// Отличия раздела визуализации (§2.10, §5.2 ТЗ): `(параметры, бюджет кэша)`.
/// Бюджет кэша — в памяти и на диске — применяется отдельной строкой §5.2.
fn visualization_differs(a: &VizSettings, b: &VizSettings) -> (bool, bool) {
    // Без `..` — новое поле `VizSettings` без строки здесь не компилируется.
    let VizSettings {
        skip_fulltrack_for_dsd: a_skip_fulltrack_for_dsd,
        viz_max_ram_mb: a_viz_max_ram_mb,
        disk_max_size_mb: a_disk_max_size_mb,
        oscilloscope: a_oscilloscope,
        spectrogram: a_spectrogram,
        spectrum: a_spectrum,
    } = a;
    let VizSettings {
        skip_fulltrack_for_dsd: b_skip_fulltrack_for_dsd,
        viz_max_ram_mb: b_viz_max_ram_mb,
        disk_max_size_mb: b_disk_max_size_mb,
        oscilloscope: b_oscilloscope,
        spectrogram: b_spectrogram,
        spectrum: b_spectrum,
    } = b;
    let params = a_skip_fulltrack_for_dsd != b_skip_fulltrack_for_dsd
        || a_oscilloscope != b_oscilloscope
        || a_spectrogram != b_spectrogram
        || a_spectrum != b_spectrum;
    let cache_budget = a_viz_max_ram_mb != b_viz_max_ram_mb || a_disk_max_size_mb != b_disk_max_size_mb;
    (params, cache_budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::settings_file::ThemeName;
    use crate::persist::state_file::{StateChange, WidthPct};
    use crate::settings::playback::{BufferMs, DeviceBuffer, Dither};

    /// Черновик, каким его создаёт открытие диалога (§6.9 `dialog_open`).
    fn draft_of(cur: &Settings, state: &SessionState) -> DialogDraft {
        DialogDraft {
            settings: cur.clone(),
            viz_mode: state.viz_mode(),
            reset_widths: false,
            shown_mode: state.active_mode(),
            theme: ThemeLoad::Unchanged,
        }
    }

    #[test]
    fn diff_untouched_draft_is_empty() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let draft = draft_of(&cur, &state);
        assert_eq!(diff(&cur, &state, &draft), SettingsDiff::default());
        assert!(diff(&cur, &state, &draft).is_empty());
    }

    #[test]
    fn diff_theme_only_reports_theme_field() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        draft.settings.theme = ThemeName::new("dark").expect("valid theme name");
        draft.theme = ThemeLoad::Loading { gen: 1 };
        let d = diff(&cur, &state, &draft);
        assert_eq!(d.fields, BTreeSet::from([SettingsField::Theme]));
        assert!(d.modes.is_none() && d.viz_mode.is_none() && !d.reset_widths);

        // Вернуть прежнюю тему — отличия нет, хотя тема перечитывалась (ТЗ-27).
        draft.settings.theme = cur.theme.clone();
        assert!(diff(&cur, &state, &draft).is_empty());
    }

    #[test]
    fn diff_reset_widths_needs_stored_widths() {
        let cur = Settings::default();
        let mut state = SessionState::default();
        let mut draft = draft_of(&cur, &state);
        draft.reset_widths = true;
        assert!(diff(&cur, &state, &draft).is_empty());

        let width = WidthPct::new(25.0).expect("valid width");
        state.apply(StateChange::ColumnWidths(BTreeMap::from([(ColumnId::ALL[0], width)])));
        let d = diff(&cur, &state, &draft);
        assert!(d.reset_widths);
        assert!(d.fields.is_empty() && d.modes.is_none() && d.viz_mode.is_none());
    }

    #[test]
    fn diff_ignores_shown_mode() {
        let (cur, state) = (Settings::default(), SessionState::default());
        for shown in [ModeKind::Compatible, ModeKind::Optimal, ModeKind::Strict] {
            let mut draft = draft_of(&cur, &state);
            draft.shown_mode = shown;
            assert!(diff(&cur, &state, &draft).is_empty());
        }
    }

    #[test]
    fn diff_mode_param_goes_to_modes_only() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        draft.settings.playback.compatible.dither = Dither::Off;
        let d = diff(&cur, &state, &draft);
        assert_eq!(d.modes, Some(cur.playback.diff(&draft.settings.playback)));
        assert!(d.modes.is_some_and(|m| m.changed.len() == 1));
        assert!(d.fields.is_empty() && d.viz_mode.is_none() && !d.reset_widths);
    }

    #[test]
    fn validate_valid_draft_is_empty() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        assert!(validate(&draft).is_empty());

        // Прочитанная тема допустима (ТЗ-29).
        let theme: ThemeData = toml::from_str(crate::theme::DEFAULT_DARK_TOML).expect("built-in theme parses");
        draft.theme = ThemeLoad::Ready(Arc::new(theme));
        assert!(validate(&draft).is_empty());
    }

    #[test]
    fn validate_loading_theme_is_invalid() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        draft.theme = ThemeLoad::Loading { gen: 7 };
        assert_eq!(validate(&draft), vec![Invalid { param: "Тема", reason: "тема загружается".into() }]);
    }

    #[test]
    fn validate_broken_theme_reports_its_reason() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        draft.theme = ThemeLoad::Invalid { reason: "строка 3: нет цвета accent".into() };
        assert_eq!(validate(&draft), vec![Invalid { param: "Тема", reason: "строка 3: нет цвета accent".into() }]);
    }

    #[test]
    fn validate_buffers_name_param_and_mode() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        // И-Р22: 100 мс < 2 × 400 мс — только в Строгом режиме.
        draft.settings.playback.strict.buffer = BufferMs::new(100).expect("valid buffer");
        draft.settings.playback.strict.device_buffer = DeviceBuffer::Ms400;
        let expected = Invalid {
            param: "Буфер устройства (Строгий режим)",
            reason: "увеличьте буфер плеера до 800 мс".into(),
        };
        assert_eq!(validate(&draft), vec![expected.clone()]);

        // Порядок: тема, Оптимальный, Строгий (§6.9: подсказка — первый).
        draft.theme = ThemeLoad::Loading { gen: 1 };
        draft.settings.playback.optimal.buffer = BufferMs::new(100).expect("valid buffer");
        draft.settings.playback.optimal.device_buffer = DeviceBuffer::Ms100;
        let found = validate(&draft);
        let params: Vec<&str> = found.iter().map(|i| i.param).collect();
        assert_eq!(params, ["Тема", "Буфер устройства (Оптимальный режим)", "Буфер устройства (Строгий режим)"]);
        assert_eq!(found.last(), Some(&expected));
    }

    #[test]
    fn apply_plan_empty_diff_is_empty() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let draft = draft_of(&cur, &state);
        let d = diff(&cur, &state, &draft);
        assert!(apply_plan(&d, &draft).is_empty());
    }

    #[test]
    fn apply_plan_theme_only_uses_read_data() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        draft.settings.theme = ThemeName::new("dark").expect("valid theme name");
        let theme: ThemeData = toml::from_str(crate::theme::DEFAULT_DARK_TOML).expect("built-in theme parses");
        let data = Arc::new(theme);
        draft.theme = ThemeLoad::Ready(Arc::clone(&data));
        let d = diff(&cur, &state, &draft);
        assert_eq!(d.fields, BTreeSet::from([SettingsField::Theme]));
        let plan = apply_plan(&d, &draft);
        assert_eq!(plan, vec![ApplyStep::Theme(Arc::clone(&data))]);
        // Те же данные, что прочитаны при выборе: повторного чтения нет (ТЗ-29).
        assert!(matches!(&plan[..], [ApplyStep::Theme(applied)] if Arc::ptr_eq(applied, &data)));

        // Данных темы нет — шага темы нет, паники нет.
        draft.theme = ThemeLoad::Unchanged;
        assert!(apply_plan(&d, &draft).is_empty());
    }

    #[test]
    fn apply_plan_column_visibility_only() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        let def = draft.settings.columns.defs.values_mut().next().expect("default settings define columns");
        def.visible = !def.visible;
        let d = diff(&cur, &state, &draft);
        let expected = ApplyStep::Columns { visibility: true, order: false, defs: false };
        assert_eq!(apply_plan(&d, &draft), vec![expected]);
    }

    #[test]
    fn apply_plan_cover_size_only_is_top_panel() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        draft.settings.top_panel.cover_size = cur.top_panel.cover_size + 1.0;
        let d = diff(&cur, &state, &draft);
        let plan = apply_plan(&d, &draft);
        assert_eq!(plan, vec![ApplyStep::TopPanel]);
        assert!(!plan.iter().any(|step| matches!(step, ApplyStep::Engine(_))));
    }

    /// `rebase` на поле, не относящемся к `playback` (SP1.0-B13, вариант А):
    /// неотредактированное пользователем значение следует новому действующему,
    /// а отредактированное — остаётся черновиковым, даже если действующее
    /// значение тоже изменилось.
    #[test]
    fn rebase_follows_unedited_field_keeps_edited_one() {
        let cur = Settings::default();
        let state = SessionState::default();
        let mut draft = draft_of(&cur, &state);

        // `minimize_to_tray` в черновике не редактировался — следует новому
        // действующему значению.
        let mut new = cur.clone();
        new.minimize_to_tray = !cur.minimize_to_tray;
        rebase(&mut draft, &cur, &new);
        assert_eq!(draft.settings.minimize_to_tray, new.minimize_to_tray);

        // `scroll_to_playing` пользователь уже изменил в черновике — внешнее
        // изменение действующего значения его не перетирает.
        draft.settings.scroll_to_playing = !cur.scroll_to_playing;
        let mut new2 = new.clone();
        new2.scroll_to_playing = !new.scroll_to_playing;
        let user_value = draft.settings.scroll_to_playing;
        rebase(&mut draft, &new, &new2);
        assert_eq!(draft.settings.scroll_to_playing, user_value);
        // Поле, которое продолжало следовать действующему значению, обновилось.
        assert_eq!(draft.settings.minimize_to_tray, new2.minimize_to_tray);
    }

    #[test]
    fn apply_plan_mode_param_sends_one_engine_step() {
        let (cur, state) = (Settings::default(), SessionState::default());
        let mut draft = draft_of(&cur, &state);
        draft.settings.playback.compatible.dither = Dither::Off;
        let d = diff(&cur, &state, &draft);
        let plan = apply_plan(&d, &draft);
        let [ApplyStep::Engine(update)] = &plan[..] else {
            panic!("expected exactly one Engine step, got {plan:?}");
        };
        assert!(update.diff.is_some());
        assert_eq!(update.diff, d.modes);
        assert_eq!(update.settings, draft.settings.playback);
    }
}
