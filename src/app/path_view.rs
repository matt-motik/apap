//! Чистый презентер окна «Путь сигнала»: `PathReport` → вид для Slint
//! (§3.6, ТЗ-71, ТЗ-81, ТЗ-84). Модуль не знает о Slint-свойствах —
//! подключение к ним — в `mod.rs` (`push_path_view_to_ui`).

use music_player_rs::audio::path::recommend::{RecAction, Recommendation};
use music_player_rs::audio::path::report::{
    confidence_text, recommendation_group, recommendation_text, stage_name, status_text, PathReport, RecGroup,
    StageRow,
};
use music_player_rs::settings::params::SettingsAnchor;

/// Строка этапа для UI (§3.6, ТЗ-71): название, статус, изменения, причины,
/// достоверность, заметки — той же формулировкой, что в `Display` отчёта.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct StageView {
    pub name: String,
    pub status: String,
    pub changes: String,
    pub causes: String,
    pub confidence: String,
    pub notes: String,
}

/// Быстрое исправление одним нажатием (ТЗ-81): рекомендация «громкость →
/// 100 %» / «выключить mute» без перехода в диалог настроек.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum QuickFix {
    Volume,
    Unmute,
}

/// Действие кнопки рекомендации (ТЗ-81): быстрое исправление, переход к
/// вкладке диалога настроек или отсутствие действия (ОС/оборудование/смена
/// режима — только текст).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RecViewAction {
    QuickFix(QuickFix),
    OpenSettings(&'static str),
    None,
}

/// Строка рекомендации для UI (ТЗ-81, ТЗ-84): текст и действие кнопки.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct RecView {
    pub text: String,
    pub action: RecViewAction,
}

/// Вид окна «Путь сигнала» для Slint (§3.6, ТЗ-71, ТЗ-81, ТЗ-84):
/// заголовок, шесть этапов, рекомендации по трём группам, подвал и
/// текст для кнопки «Скопировать» (ТЗ-84).
#[derive(Clone, PartialEq, Debug)]
pub(super) struct PathView {
    pub title: String,
    pub stages: [StageView; 6],
    pub player: Vec<RecView>,
    pub os: Vec<RecView>,
    pub hardware: Vec<RecView>,
    pub footer: String,
    pub copy_text: String,
}

/// Строит `PathView` из `PathReport` (§3.6, ТЗ-71, ТЗ-81, ТЗ-84): чистое
/// преобразование без I/O и без Slint-типов.
pub(super) fn path_view(report: &PathReport) -> PathView {
    let stages = report.stages.each_ref().map(stage_view);

    let mut player = Vec::new();
    let mut os = Vec::new();
    let mut hardware = Vec::new();
    for rec in &report.recommendations {
        let view = RecView { text: recommendation_text(rec), action: rec_action(rec) };
        match recommendation_group(rec) {
            RecGroup::Player => player.push(view),
            RecGroup::Os => os.push(view),
            RecGroup::Hardware => hardware.push(view),
        }
    }

    PathView {
        title: report.subject_title.clone(),
        stages,
        player,
        os,
        hardware,
        footer: format!(
            "{}\n\nВерсия плеера: {}\nОС: {}",
            report.guarantee_boundary, report.player_version, report.os
        ),
        copy_text: report.to_string(),
    }
}

/// Строка этапа → вид (§3.6, ТЗ-71): та же формулировка, что в `Display`
/// отчёта (`impl std::fmt::Display for PathReport`), без дублирования текста.
fn stage_view(row: &StageRow) -> StageView {
    StageView {
        name: stage_name(row.id).to_owned(),
        status: status_text(row.status).to_owned(),
        changes: row.changes.join("; "),
        causes: row.causes.join("; "),
        confidence: confidence_text(row.confidence),
        notes: row.notes.join("; "),
    }
}

/// Действие кнопки рекомендации (ТЗ-81): `PlayerSetting` с «громкость →
/// 100 %»/«выключить mute» — быстрое исправление; другая настройка плеера —
/// переход к её якорю, если он заполнен; ОС/оборудование/смена режима —
/// без действия.
fn rec_action(rec: &Recommendation) -> RecViewAction {
    match rec {
        Recommendation::PlayerSetting(r) => player_setting_action(r.action(), r.anchor()),
        Recommendation::Os { .. } | Recommendation::Hardware { .. } | Recommendation::SwitchMode { .. } => {
            RecViewAction::None
        }
    }
}

/// Действие для рекомендации «Настройки плеера» (ТЗ-81): вынесено из
/// `rec_action`, так как якоря диалога в `PARAMS` пока не заполнены
/// (решение шага AM1.0-8.5 в `recommend.rs`) и проверить ветку
/// `OpenSettings` на реальных данных сейчас нельзя.
fn player_setting_action(action: RecAction, anchor: Option<SettingsAnchor>) -> RecViewAction {
    match action {
        RecAction::SetVolumeFull => RecViewAction::QuickFix(QuickFix::Volume),
        RecAction::Unmute => RecViewAction::QuickFix(QuickFix::Unmute),
        RecAction::TurnOff | RecAction::ChooseQualityFilter | RecAction::Increase => match anchor {
            Some(a) => RecViewAction::OpenSettings(a.tab),
            None => RecViewAction::None,
        },
    }
}

#[cfg(test)]
mod tests {
    //! Тесты презентера окна «Путь сигнала» (§3.6, ТЗ-71, ТЗ-81, ТЗ-84).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

    use music_player_rs::audio::format::SampleRate;
    use music_player_rs::audio::path::recommend::{HwRequirement, OsTopic, PathSubject, PlayerSettingRec, SwitchWhy};
    use music_player_rs::audio::path::report::build_report;
    use music_player_rs::audio::path::signal::DynamicPathState;
    use music_player_rs::settings::params::ParamId;
    use music_player_rs::settings::playback::ModeSettings;
    use music_player_rs::settings::ModeKind;

    use super::*;

    fn no_stream_report() -> PathReport {
        let s = ModeSettings::default();
        build_report(PathSubject::NoStream, &DynamicPathState::default(), &s)
    }

    /// Шесть этапов окна присутствуют в `PathView` (ТЗ-71).
    #[test]
    fn path_view_has_six_stages() {
        let v = path_view(&no_stream_report());
        assert_eq!(v.stages.len(), 6);
        for stage in &v.stages {
            assert!(!stage.name.is_empty());
            assert!(!stage.status.is_empty());
        }
    }

    /// Группировка рекомендаций совпадает с `recommendation_group` (ТЗ-81).
    #[test]
    fn path_view_groups_match_recommendation_group() {
        let volume = Recommendation::PlayerSetting(
            PlayerSettingRec::new(ParamId::Volume, ModeKind::Optimal, RecAction::SetVolumeFull).unwrap(),
        );
        let os = Recommendation::Os { topic: OsTopic::PipeWireAllowedRates };
        let hardware = Recommendation::Hardware { need: HwRequirement::Rate(SampleRate::new(192_000).unwrap()) };
        let switch = Recommendation::SwitchMode { to: ModeKind::Optimal, why: SwitchWhy::Resample };

        let mut report = no_stream_report();
        report.recommendations = [volume.clone(), os.clone(), hardware.clone(), switch.clone()].into_iter().collect();
        let v = path_view(&report);

        assert_eq!(v.player.len(), 1);
        assert_eq!(v.player[0].text, recommendation_text(&volume));
        assert_eq!(v.os.len(), 1);
        assert_eq!(v.os[0].text, recommendation_text(&os));
        assert_eq!(v.hardware.len(), 2, "{:?}", v.hardware);
    }

    /// «громкость → 100 %» и «выключить mute» — быстрое исправление (ТЗ-81).
    #[test]
    fn path_view_quick_fix_volume_and_unmute() {
        let volume = PlayerSettingRec::new(ParamId::Volume, ModeKind::Optimal, RecAction::SetVolumeFull).unwrap();
        assert_eq!(player_setting_action(volume.action(), volume.anchor()), RecViewAction::QuickFix(QuickFix::Volume));

        let mute = PlayerSettingRec::new(ParamId::Mute, ModeKind::Optimal, RecAction::Unmute).unwrap();
        assert_eq!(player_setting_action(mute.action(), mute.anchor()), RecViewAction::QuickFix(QuickFix::Unmute));
    }

    /// Настройка плеера с заполненным якорем — переход к вкладке диалога
    /// (ТЗ-81). Якорь задан вручную: в `PARAMS` сейчас все `None`
    /// (решение шага AM1.0-8.5 в `recommend.rs`).
    #[test]
    fn path_view_anchored_player_setting_opens_settings() {
        let anchor = SettingsAnchor { tab: "dsd", element: "filter" };
        assert_eq!(
            player_setting_action(RecAction::ChooseQualityFilter, Some(anchor)),
            RecViewAction::OpenSettings("dsd")
        );
        assert_eq!(player_setting_action(RecAction::TurnOff, None), RecViewAction::None);
    }

    /// `copy_text` — тот же текст, что `PathReport::to_string()` (ТЗ-84).
    #[test]
    fn path_view_copy_text_matches_display() {
        let report = no_stream_report();
        let v = path_view(&report);
        assert_eq!(v.copy_text, report.to_string());
    }
}
