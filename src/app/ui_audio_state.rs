//! Зеркало состояния движка в UI-потоке (ADR-02, И-Р13, И-Р14).
//!
//! Мост С3, шаг 25: события `EngineEvent` разбираются в UI-потоке и
//! накладываются на `UiAudioState`; в Slint-свойства пишутся только
//! изменившиеся поля (И-Р13) — у каждого поля есть копия «записано в UI»
//! (`Written<T>`). Устаревшие события отбрасываются по `req_gen` команды
//! открытия (И-Р14, ТЗ-56): модуль не знает о Slint и не пишет в UI —
//! подключение к `MusicApp`/`ui_manager.rs` происходит в шаге 28 (после
//! подключения мост снимается).
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use music_player_rs::audio::backend::catalog::DeviceCatalog;
use music_player_rs::audio::decoder::TrackInfo;
use music_player_rs::audio::error::{OpenError, Reaction};
use music_player_rs::audio::player::StreamDesc;
use music_player_rs::engine::messages::{EngineEvent, Notice, SkipReason, TransportState};

/// Ячейка значения с копией «записано в UI» (ADR-02, И-Р13).
///
/// `set` обновляет текущее значение; `take_dirty` отдаёт его, только если
/// оно отличается от последнего отданного — и в этот момент фиксирует
/// текущее значение как «записанное». Повтор одного и того же значения
/// после `take_dirty` больше не считается изменением.
struct Written<T: PartialEq + Clone> {
    value: Option<T>,
    written: Option<T>,
}

impl<T: PartialEq + Clone> Default for Written<T> {
    fn default() -> Self {
        Self { value: None, written: None }
    }
}

impl<T: PartialEq + Clone> Written<T> {
    fn set(&mut self, v: T) {
        self.value = Some(v);
    }

    /// `Some(&T)`, только если `value` отличается от последней отданной копии
    /// (И-Р13); в этот момент копия обновляется, повторный вызов без нового
    /// `set` отличного значения вернёт `None`.
    fn take_dirty(&mut self) -> Option<&T> {
        if self.value != self.written {
            self.written = self.value.clone();
            self.value.as_ref()
        } else {
            None
        }
    }
}

/// `TrackInfo` не реализует `PartialEq` (§2, decoder.rs), поэтому вместо
/// сравнения по значению поле хранит снимок с номером сессии (= `req_gen`
/// принятого `Opened`): новая принятая сессия всегда «изменение», повторный
/// `Opened` с тем же `req_gen` в движке не возникает.
#[derive(Clone)]
struct TrackSnapshot {
    session: u64,
    info: Arc<TrackInfo>,
}

impl PartialEq for TrackSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
    }
}

/// `StreamDesc` аналогично `TrackInfo` не реализует `PartialEq`; сравнение —
/// по той же сессии, что и у `TrackSnapshot` (оба поля приходят одним
/// `Opened`).
#[derive(Clone)]
struct StreamSnapshot {
    session: u64,
    desc: Arc<Option<StreamDesc>>,
}

impl PartialEq for StreamSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
    }
}

/// `DeviceCatalog` не реализует `PartialEq`; движок сам шлёт `Devices`
/// только при изменении списка (ADR-16), но сравнение по `generation`
/// повторяет И-Р13 и на стороне UI, не завязываясь на идентичность `Arc`.
#[derive(Clone)]
struct DeviceSnapshot(Arc<DeviceCatalog>);

impl PartialEq for DeviceSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.0.generation == other.0.generation
    }
}

/// Результат `UiAudioState::apply` для событий, не сводимых к полю
/// состояния: вызывающий код (шаг 28) решает, что с ними делать
/// (сообщение, реакция на потерю устройства, завершение сессии).
#[derive(Clone, Debug, PartialEq)]
pub enum Applied {
    /// Событие поглощено полями состояния либо отброшено как устаревшее
    /// (И-Р14).
    None,
    Skipped { path: Arc<Path>, reason: SkipReason },
    OpenFailed { err: OpenError },
    /// Текущая сессия воспроизведения завершена.
    Ended,
    DeviceLost(Reaction),
    Notice(Notice),
    ShutdownComplete,
}

/// Зеркало состояния движка в UI-потоке (ADR-02).
///
/// `begin_open` вызывается UI при отправке `EngineCmd::Open` — до ответа
/// движка — и фиксирует новый `req_gen` как «последний запрошенный»: любое
/// `Opened`/`Skipped`/`OpenFailed` с меньшим `req_gen` относится к уже
/// переоткрытому треку и отбрасывается (И-Р14, ТЗ-56).
#[derive(Default)]
pub struct UiAudioState {
    latest_req_gen: u64,
    /// `req_gen` последнего принятого `Opened` (= id активной сессии).
    session: Option<u64>,
    transport: Written<TransportState>,
    position_secs: Written<f64>,
    track: Written<TrackSnapshot>,
    stream: Written<StreamSnapshot>,
    devices: Written<DeviceSnapshot>,
}

impl UiAudioState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Регистрирует `req_gen` команды `Open`, отправленной UI (И-Р14).
    pub fn begin_open(&mut self, req_gen: u64) {
        self.latest_req_gen = req_gen;
    }

    /// Накладывает событие движка на состояние. Возвращает `Applied::None`
    /// для событий, которые полностью свелись к полю (или были отброшены
    /// как устаревшие) — значения читаются через `take_*_dirty`.
    pub fn apply(&mut self, ev: EngineEvent) -> Applied {
        match ev {
            EngineEvent::Opened { req_gen, info, stream } => {
                if req_gen < self.latest_req_gen {
                    return Applied::None;
                }
                self.session = Some(req_gen);
                self.track.set(TrackSnapshot { session: req_gen, info: Arc::new(info) });
                self.stream.set(StreamSnapshot { session: req_gen, desc: Arc::new(stream) });
                self.position_secs.set(0.0);
                Applied::None
            }
            EngineEvent::Skipped { req_gen, path, reason } => {
                if req_gen < self.latest_req_gen {
                    return Applied::None;
                }
                Applied::Skipped { path, reason }
            }
            EngineEvent::OpenFailed { req_gen, err } => {
                if req_gen < self.latest_req_gen {
                    return Applied::None;
                }
                Applied::OpenFailed { err }
            }
            EngineEvent::Ended { session } => {
                if self.session != Some(session) {
                    return Applied::None;
                }
                Applied::Ended
            }
            EngineEvent::Transport { state } => {
                self.transport.set(state);
                Applied::None
            }
            EngineEvent::Position { secs } => {
                // Позиция отбрасывается, пока не принят `Opened` для
                // последнего запрошенного открытия: иначе на экране может
                // на мгновение мелькнуть позиция уже закрытой сессии
                // (главная цель фильтра И-Р14 для `Position`).
                if self.session != Some(self.latest_req_gen) {
                    return Applied::None;
                }
                self.position_secs.set(secs);
                Applied::None
            }
            EngineEvent::DeviceLost { reaction } => Applied::DeviceLost(reaction),
            EngineEvent::Devices(catalog) => {
                self.devices.set(DeviceSnapshot(catalog));
                Applied::None
            }
            EngineEvent::Notice(notice) => Applied::Notice(notice),
            EngineEvent::ShutdownComplete => Applied::ShutdownComplete,
        }
    }

    /// `Some`, только если транспортное состояние изменилось с прошлого
    /// чтения (И-Р13).
    pub fn take_transport_dirty(&mut self) -> Option<TransportState> {
        self.transport.take_dirty().copied()
    }

    /// `Some`, только если позиция изменилась с прошлого чтения (И-Р13).
    pub fn take_position_dirty(&mut self) -> Option<f64> {
        self.position_secs.take_dirty().copied()
    }

    /// `Some`, только при новой принятой сессии открытия.
    pub fn take_track_dirty(&mut self) -> Option<Arc<TrackInfo>> {
        self.track.take_dirty().map(|s| s.info.clone())
    }

    /// `Some`, только при новой принятой сессии открытия.
    pub fn take_stream_dirty(&mut self) -> Option<Arc<Option<StreamDesc>>> {
        self.stream.take_dirty().map(|s| s.desc.clone())
    }

    /// `Some`, только если каталог устройств сменил `generation` (ADR-16).
    pub fn take_devices_dirty(&mut self) -> Option<Arc<DeviceCatalog>> {
        self.devices.take_dirty().map(|d| d.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use music_player_rs::audio::decoder::Tags;
    use music_player_rs::engine::messages::SkipReason;
    use music_player_rs::audio::error::FileError;

    fn track_info() -> TrackInfo {
        TrackInfo {
            sample_rate: 44_100,
            channels: 2,
            num_frames: Some(1_000),
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
    fn written_cell_dirty_only_on_change() {
        let mut state = UiAudioState::new();
        let mut writes = 0;

        state.apply(EngineEvent::Transport { state: TransportState::Playing });
        if state.take_transport_dirty().is_some() {
            writes += 1;
        }

        // Повтор того же значения — не изменение (И-Р13).
        state.apply(EngineEvent::Transport { state: TransportState::Playing });
        if state.take_transport_dirty().is_some() {
            writes += 1;
        }
        state.apply(EngineEvent::Transport { state: TransportState::Playing });
        if state.take_transport_dirty().is_some() {
            writes += 1;
        }

        assert_eq!(writes, 1);

        // Настоящее изменение — снова даёт запись.
        state.apply(EngineEvent::Transport { state: TransportState::Paused });
        assert_eq!(state.take_transport_dirty(), Some(TransportState::Paused));
    }

    #[test]
    fn stale_opened_dropped_after_newer_begin_open() {
        let mut state = UiAudioState::new();

        state.begin_open(1);
        state.apply(opened(1));
        assert!(state.take_track_dirty().is_some());

        // UI запросило новое открытие — req_gen 1 теперь устарел.
        state.begin_open(2);
        let applied = state.apply(opened(1));
        assert_eq!(applied, Applied::None);
        // Устаревшее событие не меняет сессию и не взводит грязный флаг.
        assert_eq!(state.session, Some(1));
        assert!(state.take_track_dirty().is_none());
    }

    #[test]
    fn stale_skipped_and_open_failed_dropped() {
        let mut state = UiAudioState::new();
        state.begin_open(2);

        let stale = state.apply(EngineEvent::Skipped {
            req_gen: 1,
            path: Arc::from(Path::new("/tmp/a.flac")),
            reason: SkipReason::File(FileError::Corrupt(
                music_player_rs::audio::error::CorruptKind::BadHeader,
            )),
        });
        assert_eq!(stale, Applied::None);

        let stale_failed = state.apply(EngineEvent::OpenFailed {
            req_gen: 1,
            err: OpenError::DeviceLost,
        });
        assert_eq!(stale_failed, Applied::None);

        let accepted = state.apply(EngineEvent::OpenFailed { req_gen: 2, err: OpenError::DeviceLost });
        assert_eq!(accepted, Applied::OpenFailed { err: OpenError::DeviceLost });
    }

    #[test]
    fn ended_for_old_session_dropped() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened(1));

        assert_eq!(state.apply(EngineEvent::Ended { session: 0 }), Applied::None);
        assert_eq!(state.apply(EngineEvent::Ended { session: 1 }), Applied::Ended);
    }

    #[test]
    fn opened_resets_position() {
        let mut state = UiAudioState::new();
        state.begin_open(1);

        // До принятого Opened позиция для несуществующей сессии отбрасывается.
        state.apply(EngineEvent::Position { secs: 5.0 });
        assert!(state.take_position_dirty().is_none());

        state.apply(opened(1));
        assert_eq!(state.take_position_dirty(), Some(0.0));

        state.apply(EngineEvent::Position { secs: 42.0 });
        assert_eq!(state.take_position_dirty(), Some(42.0));
    }

    #[test]
    fn devices_dirty_only_when_catalog_changes() {
        let mut state = UiAudioState::new();
        let catalog_v1 = Arc::new(DeviceCatalog { shared: Vec::new(), generation: 1 });

        state.apply(EngineEvent::Devices(catalog_v1.clone()));
        assert!(state.take_devices_dirty().is_some());

        // Тот же каталог (та же generation) — не изменение.
        state.apply(EngineEvent::Devices(catalog_v1.clone()));
        assert!(state.take_devices_dirty().is_none());

        let catalog_v2 = Arc::new(DeviceCatalog { shared: Vec::new(), generation: 2 });
        state.apply(EngineEvent::Devices(catalog_v2));
        assert!(state.take_devices_dirty().is_some());
    }
}
