//! Временный мост С3 между `MusicApp` и потоком `apap-engine` (ADR-01,
//! ADR-02, ТЗ-103).
//!
//! `AudioFacade` подставляется вместо `Player` (шаг 30) и повторяет имена и
//! сигнатуры методов/поля `Player`, которые использует `src/app/*` — чтобы
//! вызывающий код не менялся. Вся реальная работа уходит через
//! `EngineHandle::send(EngineCmd)` в поток движка; видимое UI состояние
//! (транспорт, позиция, трек, поток, устройство) кэшируется здесь же из
//! входящих `EngineEvent` (`on_event`), а не читается через
//! `UiAudioState::take_*_dirty` — у геттеров фасада нет «разового» чтения.
//! `UiAudioState` используется внутри `on_event` только чтобы переиспользовать
//! её фильтр устаревших событий по `req_gen` (И-Р14, ТЗ-56): принятое
//! значение взводит `take_*_dirty`, устаревшее — нет, и кэш фасада просто
//! копирует то, что оказалось «грязным» в момент разбора этого события.
//!
//! Мост снимается на этапе С5: вызовы фасада в `src/app/*` заменяются на
//! прямую работу с `EngineHandle`/`SignalPath`/`BadgeState`; состояние
//! фасада переносится целиком, не по файлам.
#![allow(dead_code)] // часть API моста используется только тестами

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use music_player_rs::audio::decoder::TrackInfo;
use music_player_rs::audio::player::{ReservationEvent, StreamDesc};
use music_player_rs::audio::verdict::BadgeState;
use music_player_rs::engine::legacy_path::legacy_audio;
use music_player_rs::engine::messages::{EngineCmd, EngineEvent, Notice, TransportState};
use music_player_rs::engine::run::EngineHandle;
use music_player_rs::settings::params::ModeSettingsUpdate;
use music_player_rs::settings::playback::{ModeKind, ModeSettings};

use super::badge_view::{badge_view, test_view, BadgeView, TestView};
use super::ui_audio_state::{Applied, UiAudioState};

/// Безопасное преобразование числа фреймов (`u64`) в `f64` для расчёта
/// длительности трека (ТЗ-103). `as` на сэмплах запрещён правилами проекта;
/// здесь это счётчик фреймов, не сэмпл, и потеря точности `u64 -> f64`
/// (мантисса 53 бита, ~9·10^15) недостижима для реальных треков — это
/// намного больше, чем число фреймов трека на разумной частоте дискретизации.
fn frames_to_f64(frames: u64) -> f64 {
    frames as f64
}

/// Мост С3: заменяет `Player` на вызывающей стороне (ADR-01, ADR-02).
///
/// `engine: None` — команды становятся no-op (используется в тестах и как
/// деградация при провале `EngineHandle::spawn`); кэш состояния при этом
/// по-прежнему обновляется через `on_event`, т.к. источник правды для него —
/// не `self.engine`, а входящие `EngineEvent`.
pub(crate) struct AudioFacade {
    engine: Option<EngineHandle>,
    /// Только для фильтра устаревших событий по `req_gen` внутри `on_event`
    /// (И-Р14) — геттеры фасада его не читают напрямую.
    state: UiAudioState,
    /// Настройки трёх режимов — копия для расчёта `ModeSettingsDiff` между
    /// последовательными `set_mode_settings` (ОВС-14, §6.18, ADR-22).
    modes: ModeSettings,
    /// Активный режим — копия для `set_active_mode` (ТЗ-20, §6.18); стартовый
    /// `SetModeSettings`/`SetActiveMode` до первого `Open` шлёт
    /// `send_startup_policy` (шаг 20, ОВС-14, §6.27), вызывающая сторона
    /// (`MusicApp::new`) лишь вызывает его сразу после конструирования.
    active_mode: ModeKind,
    req_gen: u64,
    volume: f32,
    muted: bool,
    /// `true` после `Applied::Ended`, пока не вызван `clear_end` — повторяет
    /// `Player::ended() == at_end() && !end_ack` одним флагом.
    ended: bool,
    reservation_queue: VecDeque<ReservationEvent>,
    transport: Option<TransportState>,
    position_secs: f64,
    track: Option<Arc<TrackInfo>>,
    /// Вновь принятый `Opened`, ещё не забранный вызывающей стороной —
    /// снимается один раз через `take_opened` (ADR-02, И-Р13).
    opened: Option<Arc<TrackInfo>>,
    stream: Option<Arc<Option<StreamDesc>>>,
    /// Текущий бейдж тракта (ADR-06, ТЗ-52, ТЗ-60) — зеркало
    /// `UiAudioState::take_badge_dirty`, снимается через `take_badge_view`.
    badge: BadgeState,
    /// Истина, если `badge`/`active_mode` изменились с последнего
    /// `take_badge_view` — вид бейджа/теста нужно пересчитать и записать в
    /// UI (И-Р13, ТЗ-61, ТЗ-62).
    badge_view_dirty: bool,
    /// Зеркало `Player::device_desc` — публичное поле, не метод (сохраняет
    /// синтаксис вызывающей стороны `self.player.device_desc`).
    pub(crate) device_desc: String,
}

impl AudioFacade {
    pub(crate) fn new(
        engine: Option<EngineHandle>,
        modes: ModeSettings,
        active_mode: ModeKind,
    ) -> Self {
        Self {
            engine,
            state: UiAudioState::new(),
            modes,
            active_mode,
            req_gen: 0,
            volume: 0.8,
            muted: false,
            ended: false,
            reservation_queue: VecDeque::new(),
            transport: None,
            position_secs: 0.0,
            track: None,
            opened: None,
            stream: None,
            badge: BadgeState::NoStream,
            // `true` с самого начала: первый тик должен записать в UI
            // бейдж «Нет потока», иначе Slint-дефолт `BadgeKind.none`
            // покажет старые блоки совместимости (ТЗ-52, ТЗ-56).
            badge_view_dirty: true,
            device_desc: "none".to_string(),
        }
    }

    fn send(&self, cmd: EngineCmd) -> bool {
        self.engine.as_ref().is_some_and(|engine| engine.send(cmd))
    }

    /// Команды, которыми при старте применяется сохранённая политика режимов
    /// (ОВС-14, §6.27, ADR-22): `SetModeSettings` с `diff: None` — движок
    /// просто копирует `settings` без пересчёта `ApplyKind` (потока вывода
    /// ещё нет, переоткрывать нечего) — затем `SetActiveMode` сохранённого
    /// активного режима. Чистая функция (без `self`) — пригодна для
    /// проверки равенства с восстановленной после перезапуска политикой без
    /// постройки `AudioFacade` (ТЗ-19, И-Р14).
    fn startup_commands(modes: &ModeSettings, active: ModeKind) -> [EngineCmd; 2] {
        [
            EngineCmd::SetModeSettings(ModeSettingsUpdate { settings: modes.clone(), diff: None }),
            EngineCmd::SetActiveMode(active),
        ]
    }

    /// Отправить сохранённую политику режимов движку до первого `Open`
    /// (шаг 20, ОВС-14, §6.27, ADR-22): вызывающая сторона (`MusicApp::new`)
    /// вызывает этот метод сразу после конструирования фасада, раньше
    /// любого `open`.
    pub(crate) fn send_startup_policy(&self) {
        for cmd in Self::startup_commands(&self.modes, self.active_mode) {
            self.send(cmd);
        }
    }

    /// Разобрать событие движка, обновить кэш фасада и вернуть то, что не
    /// свелось к полю состояния (сообщения, завершение сессии, резервирование,
    /// шатдаун) — вызывающая сторона (шаг 28) решает, что с этим делать.
    pub(crate) fn on_event(&mut self, ev: EngineEvent) -> Applied {
        let applied = self.state.apply(ev);

        if let Some(info) = self.state.take_track_dirty() {
            self.opened = Some(info.clone());
            self.track = Some(info);
        }
        if let Some(stream) = self.state.take_stream_dirty() {
            self.device_desc = stream
                .as_ref()
                .as_ref()
                .map(|d| d.device.clone())
                .unwrap_or_else(|| "none".to_string());
            self.stream = Some(stream);
        }
        if let Some(transport) = self.state.take_transport_dirty() {
            self.transport = Some(transport);
        }
        if let Some(pos) = self.state.take_position_dirty() {
            self.position_secs = pos;
        }
        if let Some(b) = self.state.take_badge_dirty() {
            self.badge = b;
            self.badge_view_dirty = true;
        }

        match &applied {
            Applied::Ended => self.ended = true,
            Applied::Notice(Notice::Reservation(event)) => {
                self.reservation_queue.push_back(event.clone());
            }
            _ => {}
        }

        applied
    }

    /// Снять вновь принятый `Opened`, если он есть, — одноразовое чтение
    /// для вызывающей стороны (ADR-02, И-Р13); повторный вызов до
    /// следующего `Opened` вернёт `None`.
    pub(crate) fn take_opened(&mut self) -> Option<Arc<TrackInfo>> {
        self.opened.take()
    }

    /// Снять вид бейджа тракта и кнопки «Тест», если он устарел с прошлого
    /// чтения, — одноразовое чтение для вызывающей стороны (И-Р13, ТЗ-52,
    /// ТЗ-61, ТЗ-62). Без запущенного раннера «Теста» бейдж `test` всегда
    /// `None` — передаётся `test_view` явно.
    pub(crate) fn take_badge_view(&mut self) -> Option<(BadgeView, TestView)> {
        if self.badge_view_dirty {
            self.badge_view_dirty = false;
            Some((badge_view(&self.badge), test_view(&self.badge, self.active_mode, None)))
        } else {
            None
        }
    }

    /// Завершить сессию движка: отдать и уронить `EngineHandle` — его `Drop`
    /// шлёт `Shutdown` и join-ит поток движка.
    pub(crate) fn shutdown(&mut self) {
        self.engine.take();
    }

    // --- Открытие и транспорт ---------------------------------------------

    /// Открыть трек асинхронно: команда уходит в движок немедленно, сам
    /// `TrackInfo` приходит позже событием `Opened`/`OpenFailed`/`Skipped`
    /// через `on_event` (ADR-02). Единственное отличие сигнатуры от
    /// `Player::open` (который блокирует и возвращает `TrackInfo` сразу) —
    /// шаг 28 адаптирует вызывающий код в `playback_manager.rs` под это
    /// изменение (ТЗ-56, И-Р14).
    pub(crate) fn open(&mut self, path: &Path) -> Result<(), String> {
        self.req_gen += 1;
        let req_gen = self.req_gen;
        self.state.begin_open(req_gen);
        self.ended = false;
        let cmd = EngineCmd::Open {
            req_gen,
            path: Arc::from(path),
            start_secs: 0.0,
            autoplay: false,
        };
        if self.send(cmd) {
            Ok(())
        } else {
            Err("движок не запущен".to_string())
        }
    }

    pub(crate) fn play(&mut self) {
        self.send(EngineCmd::Play);
    }

    pub(crate) fn stop(&mut self) {
        self.send(EngineCmd::Stop);
    }

    pub(crate) fn toggle(&mut self) {
        if self.is_playing() {
            self.send(EngineCmd::Pause);
        } else {
            self.send(EngineCmd::Play);
        }
    }

    pub(crate) fn seek(&mut self, secs: f64) {
        self.send(EngineCmd::Seek { secs });
    }

    pub(crate) fn is_playing(&self) -> bool {
        self.transport == Some(TransportState::Playing)
    }

    /// Истина, если транспорт остановлен (§6.18, ТЗ-20): отсутствие
    /// события `Transport` (`None`, трек ещё не открывался) трактуется как
    /// остановленное состояние — переоткрывать и восстанавливать позицию
    /// в этом случае не нужно.
    pub(crate) fn is_stopped(&self) -> bool {
        matches!(self.transport, None | Some(TransportState::Stopped))
    }

    pub(crate) fn has_decoder(&self) -> bool {
        self.track.is_some()
    }

    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    pub(crate) fn clear_end(&mut self) {
        self.ended = false;
    }

    /// `(playing, pos_secs, duration_secs)` — повторяет `Player::snapshot`.
    pub(crate) fn snapshot(&self) -> (bool, f64, Option<f64>) {
        (self.is_playing(), self.position_secs, self.duration())
    }

    fn duration(&self) -> Option<f64> {
        let info = self.track.as_ref()?;
        let frames = info.num_frames?;
        Some(frames_to_f64(frames) / f64::from(info.sample_rate))
    }

    // --- Резервирование устройства ------------------------------------------

    /// Движок публикует только терминальные `Notice::Reservation`
    /// (`Opened`/`Failed`/`Lost`), промежуточного состояния «ожидание» он
    /// не шлёт — поэтому мост всегда возвращает `false` (известное
    /// ограничение моста С3, ТЗ-119).
    pub(crate) fn reservation_pending(&self) -> bool {
        false
    }

    pub(crate) fn poll_reservation(&mut self) -> Option<ReservationEvent> {
        self.reservation_queue.pop_front()
    }

    /// Освобождает монопольный узел `hw:` без перемотки — конец плейлиста и
    /// уход в трей (V5.1-B5/B6); `Stop`/`Pause` движок освобождает сам.
    /// Мост С3: команда `ReleaseExclusive`, в С6 — `release` бэкенда (ТЗ-48).
    pub(crate) fn release_if_exclusive(&mut self) {
        self.send(EngineCmd::ReleaseExclusive);
    }

    // --- Устройство вывода ---------------------------------------------------

    pub(crate) fn set_preferred_device(&mut self, name: String) {
        self.send(EngineCmd::SetDevice { id: name });
    }

    pub(crate) fn stream_desc(&self) -> Option<&StreamDesc> {
        self.stream.as_deref()?.as_ref()
    }

    /// `(sample_rate, channels)` — из последнего известного `StreamDesc`,
    /// либо дефолт до первого открытия (повторяет `Player::format`).
    pub(crate) fn format(&self) -> (u32, usize) {
        match self.stream_desc() {
            Some(desc) => (desc.rate, usize::from(desc.channels)),
            None => (44_100, 2),
        }
    }

    // --- Громкость/mute -------------------------------------------------------

    pub(crate) fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        self.send(EngineCmd::SetVolume(self.volume));
    }

    pub(crate) fn volume(&self) -> f32 {
        self.volume
    }

    pub(crate) fn set_muted(&mut self, m: bool) {
        self.muted = m;
        self.send(EngineCmd::SetMuted(m));
    }

    pub(crate) fn muted(&self) -> bool {
        self.muted
    }

    pub(crate) fn toggle_mute(&mut self) {
        self.set_muted(!self.muted);
    }

    // --- Bit-perfect / dither -------------------------------------------------
    //
    // Флаги этого раздела не кэшируются отдельным полем: они выводятся из
    // активного режима через `legacy_audio(&self.modes, self.active_mode)` —
    // то же отображение `ModeSettings -> LegacyAudio`, которым пользуется сам
    // движок (ADR-04, ТЗ-52, §6.18). `bit_perfect` истинен только для
    // Строгого режима.

    fn legacy_bit_perfect(&self) -> bool {
        legacy_audio(&self.modes, self.active_mode).0.bit_perfect
    }

    pub(crate) fn bit_perfect(&self) -> bool {
        self.legacy_bit_perfect()
    }

    pub(crate) fn bit_perfect_resampled(&self) -> bool {
        self.legacy_bit_perfect() && self.stream_desc().is_some_and(|d| d.resampled)
    }

    // --- Настройки режимов (С4) ------------------------------------------------

    /// Применить новые настройки трёх режимов (ОВС-14, ОВС-17, §6.18, §6.27,
    /// ADR-22): движок получает копию `settings` и по `diff.strongest(active)`
    /// решает, переоткрывать поток или только обновить копию. Diff считается
    /// от предыдущей сохранённой копии (`ModeSettings::diff`); пустое
    /// множество изменений не отправляется (§2.2).
    pub(crate) fn set_mode_settings(&mut self, settings: ModeSettings) {
        if settings == self.modes {
            return;
        }
        let diff = self.modes.diff(&settings);
        self.modes = settings.clone();
        self.send(EngineCmd::SetModeSettings(ModeSettingsUpdate { settings, diff: Some(diff) }));
    }

    /// Настройки режимов уже применены «Сохранить» диалога (§6.9, ADR-11,
    /// ТЗ-28, ТЗ-29): команду `EngineCmd::SetModeSettings` движку отправило
    /// ядро в `AppCore::dialog_save`, повторно она не шлётся. Фасад только
    /// синхронизирует свою копию — от неё считается следующий diff
    /// `set_mode_settings` и от неё зависят геттеры (`bit_perfect` и пр.).
    pub(crate) fn mode_settings_applied(&mut self, settings: ModeSettings) {
        self.modes = settings;
    }

    /// Переключить активный режим (ТЗ-20, ОВ-10, §6.18): движок только
    /// сохраняет параметры нового режима (шаг 13, ТС-9) — переоткрытие с его
    /// параметрами делает последующий `Open` той же позиции (§6.18, «Смена
    /// режима»).
    pub(crate) fn set_active_mode(&mut self, mode: ModeKind) {
        if mode != self.active_mode {
            self.active_mode = mode;
            // Доступность «Теста» зависит от режима (ТЗ-62) — вид бейджа/
            // теста нужно пересчитать и записать в UI заново.
            self.badge_view_dirty = true;
            self.send(EngineCmd::SetActiveMode(mode));
        }
    }

    // --- Визуализация ----------------------------------------------------------

    pub(crate) fn set_viz_tap(&mut self, tap: Option<rtrb::Producer<f32>>) {
        self.send(EngineCmd::AttachVizTap(tap));
    }

    pub(crate) fn set_viz_tap_active(&mut self, active: bool) {
        self.send(EngineCmd::SetVizTap(active));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::BadgeKind;
    use music_player_rs::audio::decoder::Tags;
    use music_player_rs::persist::keys::{FileRead, Parsed};
    use music_player_rs::persist::state_file::parse_state;
    use music_player_rs::settings::playback_dto::{load_mode_settings, playback_dto, PlaybackDto};

    fn facade() -> AudioFacade {
        AudioFacade::new(None, ModeSettings::default(), ModeKind::default())
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

    fn opened(req_gen: u64) -> EngineEvent {
        EngineEvent::Opened {
            req_gen,
            info: track_info(),
            stream: Some(StreamDesc::default()),
            path: None,
            badge: BadgeState::NoStream,
        }
    }

    #[test]
    fn bit_perfect_follows_active_mode() {
        let mut f = facade();
        // Совместимый (дефолт) — не bit-perfect.
        assert!(!f.bit_perfect());

        f.set_active_mode(ModeKind::Strict);
        assert!(f.bit_perfect());

        f.set_active_mode(ModeKind::Optimal);
        assert!(!f.bit_perfect());
    }

    #[test]
    fn bit_perfect_resampled_false_without_stream_desc() {
        let f = facade();
        assert!(f.stream_desc().is_none());
        assert!(!f.bit_perfect_resampled());
    }

    #[test]
    fn setters_update_getters() {
        let mut f = facade();

        f.set_volume(2.0);
        assert_eq!(f.volume(), 1.0);

        f.set_muted(true);
        assert!(f.muted());
        f.toggle_mute();
        assert!(!f.muted());
    }

    #[test]
    fn on_event_opened_transport_position_update_snapshot() {
        let mut f = facade();
        let _ = f.open(Path::new("/tmp/a.flac"));
        f.on_event(opened(1));
        f.on_event(EngineEvent::Transport { state: TransportState::Playing });
        f.on_event(EngineEvent::Position { secs: 12.5 });

        let (playing, pos, dur) = f.snapshot();
        assert!(playing);
        assert_eq!(pos, 12.5);
        assert_eq!(dur, Some(10.0));
        assert!(f.has_decoder());
    }

    #[test]
    fn ended_flag_set_and_cleared() {
        let mut f = facade();
        let _ = f.open(Path::new("/tmp/a.flac"));
        f.on_event(opened(1));
        assert!(!f.ended());

        let applied = f.on_event(EngineEvent::Ended { session: 1 });
        assert_eq!(applied, Applied::Ended);
        assert!(f.ended());

        f.clear_end();
        assert!(!f.ended());
    }

    #[test]
    fn stale_opened_event_is_ignored() {
        let mut f = facade();
        let _ = f.open(Path::new("/tmp/a.flac")); // req_gen = 1
        let _ = f.open(Path::new("/tmp/b.flac")); // req_gen = 2, устаревает 1

        f.on_event(opened(1));
        // Устаревшее открытие не должно взвести кэш трека/потока.
        assert!(!f.has_decoder());
        assert_eq!(f.snapshot().2, None);
    }

    #[test]
    fn take_opened_returns_once_after_opened_event() {
        let mut f = facade();
        let _ = f.open(Path::new("/tmp/a.flac"));
        assert!(f.take_opened().is_none());

        f.on_event(opened(1));
        assert!(f.take_opened().is_some());
        assert!(f.take_opened().is_none());
    }

    #[test]
    fn reservation_notice_queued_and_polled_once() {
        let mut f = facade();
        assert!(f.poll_reservation().is_none());

        f.on_event(EngineEvent::Notice(Notice::Reservation(ReservationEvent::Opened)));
        assert_eq!(f.poll_reservation(), Some(ReservationEvent::Opened));
        assert!(f.poll_reservation().is_none());
    }

    #[test]
    fn set_mode_settings_updates_cache_and_skips_noop() {
        let mut f = facade();
        let initial = f.modes.clone();

        // Та же конфигурация — пустой diff, кэш не трогаем (§2.2).
        f.set_mode_settings(initial.clone());
        assert_eq!(f.modes, initial);

        let mut changed = initial;
        changed.compatible.dsd_gain_comp = !changed.compatible.dsd_gain_comp;
        f.set_mode_settings(changed.clone());
        assert_eq!(f.modes, changed);
    }

    /// §6.9, ADR-11: после `mode_settings_applied` копия фасада равна
    /// применённым настройкам, поэтому следующий diff считается от них:
    /// повторный `set_mode_settings` с тем же значением — пустой diff.
    #[test]
    fn mode_settings_applied_updates_cache_only() {
        let mut f = facade();
        let initial = f.modes.clone();

        let mut changed = initial.clone();
        changed.optimal.volume_lock = !changed.optimal.volume_lock;
        f.mode_settings_applied(changed.clone());
        assert_eq!(f.modes, changed);
        assert!(f.modes.diff(&changed).changed.is_empty());
        assert!(!f.modes.diff(&initial).changed.is_empty());

        f.set_mode_settings(changed.clone());
        assert_eq!(f.modes, changed);
    }

    #[test]
    fn set_active_mode_updates_cache_once() {
        let mut f = facade();
        assert_eq!(f.active_mode, ModeKind::default());

        f.set_active_mode(ModeKind::Strict);
        assert_eq!(f.active_mode, ModeKind::Strict);

        // Повторная установка того же режима — не меняет кэш (нет эффекта).
        f.set_active_mode(ModeKind::Strict);
        assert_eq!(f.active_mode, ModeKind::Strict);
    }

    /// И-Р13: свежий фасад взводит вид бейджа один раз («Нет потока» до
    /// первого `Opened`), повторное чтение без изменений — `None`.
    #[test]
    fn badge_view_written_once_until_change() {
        let mut f = facade();

        let (badge, _test) = f.take_badge_view().expect("первое чтение взведено");
        assert_eq!(badge.kind, BadgeKind::NoStream);

        assert!(f.take_badge_view().is_none());
    }

    /// ТЗ-62: доступность «Теста» зависит от режима — смена активного
    /// режима снова взводит вид бейджа/теста даже без нового `Opened`.
    #[test]
    fn badge_view_redirtied_on_mode_change() {
        let mut f = facade();
        assert!(f.take_badge_view().is_some());
        assert!(f.take_badge_view().is_none());

        f.set_active_mode(ModeKind::Strict);
        assert!(f.take_badge_view().is_some());
        assert!(f.take_badge_view().is_none());
    }

    /// ТЗ-19, И-Р14: после перезапуска стартовые команды (`startup_commands`),
    /// построенные из `ModeSettings`/активного режима, прочитанных обратно из
    /// реальных форматов персистентности (без участия движка), совпадают с
    /// тем, что было бы отправлено из значений до перезапуска (ОВС-14,
    /// §6.27, ADR-22).
    #[test]
    fn startup_policy_equals_saved_policy() {
        let mut original = ModeSettings::default();
        original.compatible.dsd_gain_comp = !original.compatible.dsd_gain_comp;
        original.optimal.volume_lock = !original.optimal.volume_lock;

        // Политика режимов — через тот же DTO/TOML, что и `settings.toml`.
        let dto = playback_dto(&original);
        let toml_text = toml::to_string(&dto).expect("сериализация PlaybackDto в toml");
        let reloaded_dto: PlaybackDto = toml::from_str(&toml_text).expect("разбор toml в PlaybackDto");
        let (reloaded_modes, _notes) = load_mode_settings(Some(reloaded_dto));
        assert_eq!(reloaded_modes, original);

        // Активный режим — через реальный парсер `state.toml`.
        let state_bytes: Arc<[u8]> = Arc::from(b"playback.active = \"strict\"\n".as_slice());
        let reloaded_active = match parse_state(FileRead::Bytes(state_bytes)) {
            Parsed::Parsed { value, .. } => value.active_mode(),
            _ => panic!("ожидался Parsed::Parsed"),
        };
        assert_eq!(reloaded_active, ModeKind::Strict);

        let original_cmds = AudioFacade::startup_commands(&original, ModeKind::Strict);
        let reloaded_cmds = AudioFacade::startup_commands(&reloaded_modes, reloaded_active);

        for cmds in [&original_cmds, &reloaded_cmds] {
            match (&cmds[0], &cmds[1]) {
                (EngineCmd::SetModeSettings(update), EngineCmd::SetActiveMode(mode)) => {
                    assert!(update.diff.is_none());
                    assert_eq!(update.settings, original);
                    assert_eq!(*mode, ModeKind::Strict);
                }
                other => panic!("ожидались SetModeSettings/SetActiveMode, получено: {other:?}"),
            }
        }
    }
}
