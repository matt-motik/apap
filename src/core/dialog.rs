//! Модель диалога настроек без Slint (ADR-11, ADR-19, §2.10): черновик,
//! отличия черновика от действующих значений и недопустимые значения
//! (ТЗ-25, ТЗ-27, ТЗ-29, ТЗ-30).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::audio::visualizer::{VisualizationMode, VizSettings};
use crate::persist::settings_file::{ColumnDef, Settings};
use crate::persist::state_file::SessionState;
use crate::settings::ColumnId;
use crate::settings::params::ModeSettingsDiff;
use crate::settings::playback::{buffers_allowed, ModeKind, ModeSettings, ValueBlocker};
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
    let ModeSettings { compatible: _, optimal, strict } = &draft.settings.playback;
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
}
