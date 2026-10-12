//! Модель диалога настроек без Slint (ADR-11, ADR-19, §2.10): черновик,
//! отличия черновика от действующих значений и недопустимые значения
//! (ТЗ-25, ТЗ-27, ТЗ-29, ТЗ-30).

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::audio::visualizer::VisualizationMode;
use crate::persist::settings_file::Settings;
use crate::settings::params::ModeSettingsDiff;
use crate::settings::playback::ModeKind;
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
