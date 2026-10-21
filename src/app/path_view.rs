//! Чистый презентер окна «Путь сигнала»: `PathReport` → вид для Slint
//! (§3.6, ТЗ-71, ТЗ-81, ТЗ-84). Модуль не знает о Slint-свойствах —
//! подключение к ним — в `mod.rs` (`push_path_view_to_ui`).

use music_player_rs::audio::path::recommend::{PathSubject, RecAction, Recommendation};
use music_player_rs::audio::path::report::{
    build_report, confidence_text, recommendation_group, recommendation_text, stage_name, status_text, PathReport,
    RecGroup, StageRow,
};
use music_player_rs::settings::params::SettingsAnchor;
use music_player_rs::settings::playback::ModeSettings;

use super::ui_audio_state::UiAudioState;

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
        RecAction::TurnOn | RecAction::TurnOff | RecAction::ChooseQualityFilter | RecAction::Increase => match anchor {
            Some(a) => RecViewAction::OpenSettings(a.tab),
            None => RecViewAction::None,
        },
    }
}

/// Кэш отчёта окна «Путь сигнала» (ТЗ-79, ТЗ-83, §7.2): предмет отчёта
/// (путь/пропуск/нет потока) и настройки режимов передаёт вызывающая
/// сторона при каждом обращении — сам кэш хранит только построенный
/// `PathReport` и факт его устаревания, взводимый `invalidate` (ADR-02).
pub(super) struct PathWindowCache {
    report: Option<PathReport>,
    /// Истина, если `report` устарел с последнего пересчёта: взводится
    /// вызывающей стороной через `invalidate` (новый/пропущенный/неоткрытый
    /// путь, изменения настроек режимов/активного режима) и снимается
    /// только построением отчёта (ТЗ-79, ТЗ-83, §7.2, тест
    /// `window_no_recompute_on_idle_ticks`).
    dirty: bool,
    /// Счётчик фактических пересчётов отчёта — только для теста
    /// `window_no_recompute_on_idle_ticks` (§7.2).
    #[cfg(test)]
    builds: usize,
}

impl PathWindowCache {
    /// `dirty: true` с самого начала — первое чтение (`get`/`take_dirty`)
    /// должно построить отчёт «Поток не открыт» (ТЗ-79, §7.2).
    pub(super) fn new() -> Self {
        Self {
            report: None,
            dirty: true,
            #[cfg(test)]
            builds: 0,
        }
    }

    /// Взвести пересчёт отчёта (ТЗ-79, ТЗ-83, §7.2): вызывается при смене
    /// предмета отчёта (новый/пропущенный/неоткрытый путь) или настроек
    /// режимов/активного режима, от которых зависят рекомендации.
    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Пересчитать `report` — единственное место, которое взводит
    /// `builds` (ТЗ-79, ТЗ-83, §7.2).
    fn rebuild(&mut self, audio: &UiAudioState, modes: &ModeSettings) {
        self.report = Some(compute_report(audio, modes));
        self.dirty = false;
        #[cfg(test)]
        {
            self.builds += 1;
        }
    }

    /// Снять отчёт, если он устарел с прошлого чтения, — одноразовое чтение
    /// для вызывающей стороны (ТЗ-79, ТЗ-83, §7.2): повторный вызов без
    /// нового `invalidate` вернёт `None`, а не пересчитает отчёт заново
    /// (тест `window_no_recompute_on_idle_ticks`).
    pub(super) fn take_dirty(&mut self, audio: &UiAudioState, modes: &ModeSettings) -> Option<&PathReport> {
        if self.dirty {
            self.rebuild(audio, modes);
            self.report.as_ref()
        } else {
            None
        }
    }

    /// Текущий отчёт, построенный при необходимости — для момента открытия
    /// окна, когда нужен готовый снимок независимо от того, был ли он уже
    /// снят через `take_dirty` (ТЗ-79, §7.2).
    pub(super) fn get(&mut self, audio: &UiAudioState, modes: &ModeSettings) -> &PathReport {
        if self.dirty || self.report.is_none() {
            self.rebuild(audio, modes);
        }
        self.report.get_or_insert_with(|| compute_report(audio, modes))
    }
}

impl Default for PathWindowCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Собрать `PathReport` из предмета (пропуск/путь/нет потока), текущей
/// динамики и настроек режимов (ТЗ-79, ТЗ-83, §7.2).
fn compute_report(audio: &UiAudioState, modes: &ModeSettings) -> PathReport {
    let subject = if let Some(blocked) = audio.skipped() {
        PathSubject::Skipped(blocked)
    } else if let Some(path) = audio.path() {
        PathSubject::Opened(path.as_ref())
    } else {
        PathSubject::NoStream
    };
    build_report(subject, audio.dynamics(), modes)
}

#[cfg(test)]
mod tests {
    //! Тесты презентера окна «Путь сигнала» (§3.6, ТЗ-71, ТЗ-81, ТЗ-84).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

    use std::path::Path;
    use std::sync::Arc;

    use music_player_rs::audio::decoder::{Tags, TrackInfo};
    use music_player_rs::audio::format::SampleRate;
    use music_player_rs::audio::path::recommend::{HwRequirement, OsTopic, PlayerSettingRec, SwitchWhy};
    use music_player_rs::audio::path::signal::DynamicPathState;
    use music_player_rs::audio::verdict::BadgeState;
    use music_player_rs::engine::messages::{EngineEvent, SkipReason};
    use music_player_rs::settings::params::ParamId;
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
        let hardware =
            Recommendation::Hardware { need: HwRequirement::Rate(SampleRate::new(192_000).unwrap()), have: None };
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

    fn track_info() -> TrackInfo {
        TrackInfo {
            sample_rate: 44_100,
            channels: 2,
            num_frames: Some(441_000),
            format_name: "flac".to_string(),
            bitrate: 0,
            bits: Some(16),
            tags: Tags::default(),
        }
    }

    fn opened_event(req_gen: u64) -> EngineEvent {
        EngineEvent::Opened {
            req_gen,
            info: track_info(),
            stream_format: None,
            path: None,
            badge: BadgeState::NoStream,
        }
    }

    /// ТЗ-79, ТЗ-83, §7.2: окно «Путь сигнала» получает отчёт из готового
    /// кэша — за 100 холостых тиков без `invalidate` не пересчитывает его
    /// повторно (`builds` остаётся 1).
    #[test]
    fn window_no_recompute_on_idle_ticks() {
        let state = UiAudioState::new();
        let modes = ModeSettings::default();
        let mut cache = PathWindowCache::new();

        let _ = cache.get(&state, &modes);
        assert_eq!(cache.builds, 1);

        for _ in 0..100 {
            assert!(cache.take_dirty(&state, &modes).is_none());
        }
        assert_eq!(cache.builds, 1);
    }

    /// ТЗ-79, ТЗ-83, §7.2: `invalidate` взводит пересчёт отчёта
    /// (рекомендации зависят от настроек режимов/активного режима) — так
    /// мост С3 помечает кэш при смене активного режима, даже без нового
    /// `Opened`.
    #[test]
    fn report_rebuilt_after_mode_change() {
        let state = UiAudioState::new();
        let modes = ModeSettings::default();
        let mut cache = PathWindowCache::new();

        let _ = cache.get(&state, &modes);
        assert_eq!(cache.builds, 1);

        cache.invalidate();
        assert!(cache.take_dirty(&state, &modes).is_some());
        assert_eq!(cache.builds, 2);
    }

    /// ТЗ-79, ТЗ-83, §7.2: принятое событие `Opened` взводит пересчёт
    /// отчёта (новый предмет окна), даже если сам путь (`SignalPath`) ещё
    /// не готов.
    #[test]
    fn report_rebuilt_after_opened_event() {
        let mut state = UiAudioState::new();
        let modes = ModeSettings::default();
        let mut cache = PathWindowCache::new();

        let _ = cache.get(&state, &modes);
        assert_eq!(cache.builds, 1);

        state.begin_open(1);
        state.apply(opened_event(1));
        // Новый путь (включая переход к `None`) — предмет отчёта сменился
        // независимо от причины (ТЗ-79, ТЗ-83, §7.2); так это взводит
        // `on_event` моста С3.
        assert!(state.take_path_dirty().is_some());
        cache.invalidate();

        assert!(cache.take_dirty(&state, &modes).is_some());
        assert_eq!(cache.builds, 2);
    }

    /// ТЗ-79, ТЗ-80, §7.2: `Skipped(Incompatible)` даёт предмет
    /// `PathSubject::Skipped` — заголовок отчёта отличается от «Поток не
    /// открыт» (`PathSubject::NoStream`).
    #[test]
    fn report_subject_skipped_differs_from_no_stream() {
        use music_player_rs::audio::backend::{AccessKind, DeviceCaps, OutputApi, RateSet};
        use music_player_rs::audio::format::{
            BitDepth, ChannelLayout, Codec, Container, OutputSampleFormat, SampleRate, SourceFormat, SourceKind,
        };
        use music_player_rs::audio::path::plan::plan;
        use music_player_rs::settings::playback::PathPolicy;

        let mut state = UiAudioState::new();
        let modes = ModeSettings::default();
        let mut cache = PathWindowCache::new();
        let no_stream_title = cache.get(&state, &modes).subject_title.clone();

        // Устройство только на 96 кГц, источник 192 кГц — Строгий режим
        // отвергает ресемплинг, план останавливается на этапе «Выход»
        // (повторяет фикстуру `report_built_without_backend_access`,
        // src/audio/path/report.rs).
        let caps = DeviceCaps {
            api: OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            name: "test".to_owned(),
            rates: RateSet::Discrete(smallvec::SmallVec::from_slice(&[
                SampleRate::new(96_000).expect("96000 — валидная частота")
            ])),
            formats: smallvec::SmallVec::from_slice(&[OutputSampleFormat::S32Le]),
            channels: smallvec::SmallVec::from_slice(&[2]),
        };
        let src = SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: SampleRate::new(192_000).expect("192000 — валидная частота"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: BitDepth::new(24).expect("24 бита — валидная разрядность") },
        };
        let settings = ModeSettings::default();
        let blocked = plan(&src, &caps, PathPolicy::Strict(&settings.strict))
            .expect_err("96 кГц устройство должно отвергнуть 192 кГц источник в Строгом режиме");

        state.begin_open(1);
        state.apply(EngineEvent::Skipped {
            req_gen: 1,
            path: Arc::from(Path::new("/tmp/a.flac")),
            reason: SkipReason::Incompatible(blocked),
        });
        cache.invalidate();

        let report = cache.get(&state, &modes);
        assert_ne!(report.subject_title, no_stream_title);
    }
}
