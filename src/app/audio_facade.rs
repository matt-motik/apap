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
use music_player_rs::engine::messages::{EngineCmd, EngineEvent, LegacyAudio, Notice, TransportState};
use music_player_rs::engine::run::EngineHandle;
use music_player_rs::settings::params::ModeSettingsUpdate;
use music_player_rs::settings::playback::{ModeKind, ModeSettings};
use music_player_rs::settings::{
    clamp_ring_buffer_ms, ClockFamily, DsdMode, ExclusiveMode, FallbackPolicy, FallbackRatePolicy,
    ResamplerAlgorithm, ResamplerDither, ResamplerMode,
};

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
    legacy: LegacyAudio,
    /// Настройки трёх режимов — копия для расчёта `ModeSettingsDiff` между
    /// последовательными `set_mode_settings` (ОВС-14, §6.18, ADR-22).
    modes: ModeSettings,
    /// Активный режим — копия для `set_active_mode` (ТЗ-20, §6.18); отправка
    /// стартового `SetModeSettings`/`SetActiveMode` до первого `Open` —
    /// задача вызывающей стороны (шаг 20, ОВС-14, §6.27).
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
    /// Зеркало `Player::device_desc` — публичное поле, не метод (сохраняет
    /// синтаксис вызывающей стороны `self.player.device_desc`).
    pub(crate) device_desc: String,
}

impl AudioFacade {
    pub(crate) fn new(
        engine: Option<EngineHandle>,
        legacy: LegacyAudio,
        modes: ModeSettings,
        active_mode: ModeKind,
    ) -> Self {
        Self {
            engine,
            state: UiAudioState::new(),
            legacy,
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
            device_desc: "none".to_string(),
        }
    }

    fn send(&self, cmd: EngineCmd) -> bool {
        self.engine.as_ref().is_some_and(|engine| engine.send(cmd))
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

    /// Мост С3: движок сам использует текущий путь/позицию открытой сессии
    /// (`Engine::handle`, `EngineCmd::SetDevice`) — `path`/`pos` сохранены
    /// только для совместимости сигнатуры с `Player::set_device`; шаг 28
    /// убирает их на вызывающей стороне.
    pub(crate) fn set_device(&mut self, name: String, _path: Option<&Path>, _pos: f64) -> Result<(), String> {
        if self.send(EngineCmd::SetDevice { id: name }) {
            Ok(())
        } else {
            Err("движок не запущен".to_string())
        }
    }

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
    // Сеттеры этого раздела и раздела ниже больше не шлют `SetLegacyAudio`
    // движку (С4, §6.18, §6.27): `LegacyAudio` строится внутри движка из
    // `ModeSettings` (`engine::legacy_path::legacy_audio`), а не из команд UI.
    // Поле `legacy` остаётся только локальным кэшем старого диалога настроек
    // (ТЗ-134, И-Р24) до его замены диалогом режимов (шаги 21…27) и удаления
    // (шаги 28…32).

    pub(crate) fn set_bit_perfect(&mut self, enabled: bool) {
        self.legacy.bit_perfect = enabled;
    }

    pub(crate) fn bit_perfect(&self) -> bool {
        self.legacy.bit_perfect
    }

    pub(crate) fn bit_perfect_resampled(&self) -> bool {
        self.legacy.bit_perfect && self.stream_desc().is_some_and(|d| d.resampled)
    }

    pub(crate) fn set_dither(&mut self, dither: ResamplerDither) {
        self.legacy.dither = dither;
    }

    // --- Прочие легаси-настройки звука (ADR-04, ТЗ-134, И-Р24) ----------------

    pub(crate) fn set_resampler_mode(&mut self, mode: ResamplerMode) {
        self.legacy.resampler_mode = mode;
    }

    pub(crate) fn set_resampler_algorithm(&mut self, algo: ResamplerAlgorithm) {
        self.legacy.resampler_algorithm = algo;
    }

    pub(crate) fn set_fixed_rate(&mut self, rate: u32) {
        self.legacy.fixed_rate = rate;
    }

    pub(crate) fn set_prefer_family(&mut self, family: ClockFamily) {
        self.legacy.prefer_family = family;
    }

    pub(crate) fn set_fallback_rate(&mut self, policy: FallbackRatePolicy) {
        self.legacy.fallback_rate = policy;
    }

    pub(crate) fn set_ring_buffer_ms(&mut self, ms: u32) {
        self.legacy.ring_buffer_ms = clamp_ring_buffer_ms(ms);
    }

    pub(crate) fn set_exclusive_mode(&mut self, mode: ExclusiveMode) {
        self.legacy.exclusive_mode = mode;
    }

    pub(crate) fn set_fallback_policy(&mut self, policy: FallbackPolicy) {
        self.legacy.fallback_policy = policy;
    }

    pub(crate) fn set_dsd_mode(&mut self, mode: DsdMode) {
        self.legacy.dsd_mode = mode;
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

    /// Переключить активный режим (ТЗ-20, ОВ-10, §6.18): движок только
    /// сохраняет параметры нового режима (шаг 13, ТС-9) — переоткрытие с его
    /// параметрами делает последующий `Open` той же позиции (§6.18, «Смена
    /// режима»).
    pub(crate) fn set_active_mode(&mut self, mode: ModeKind) {
        if mode != self.active_mode {
            self.active_mode = mode;
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
    use music_player_rs::audio::decoder::Tags;

    fn legacy_defaults() -> LegacyAudio {
        LegacyAudio {
            exclusive_mode: ExclusiveMode::default(),
            fallback_policy: FallbackPolicy::default(),
            dsd_mode: DsdMode::default(),
            resampler_mode: ResamplerMode::default(),
            resampler_algorithm: ResamplerAlgorithm::default(),
            fixed_rate: 48_000,
            prefer_family: ClockFamily::default(),
            fallback_rate: FallbackRatePolicy::default(),
            ring_buffer_ms: 1500,
            bit_perfect: false,
            dither: ResamplerDither::default(),
        }
    }

    fn facade() -> AudioFacade {
        AudioFacade::new(None, legacy_defaults(), ModeSettings::default(), ModeKind::default())
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
        EngineEvent::Opened { req_gen, info: track_info(), stream: Some(StreamDesc::default()) }
    }

    #[test]
    fn setters_update_legacy_copy_and_getters() {
        let mut f = facade();
        assert!(!f.bit_perfect());
        f.set_bit_perfect(true);
        assert!(f.bit_perfect());

        f.set_dsd_mode(DsdMode::Native);
        assert_eq!(f.legacy.dsd_mode, DsdMode::Native);

        f.set_ring_buffer_ms(50);
        assert_eq!(f.legacy.ring_buffer_ms, music_player_rs::settings::RING_BUFFER_MS_MIN);

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
}
