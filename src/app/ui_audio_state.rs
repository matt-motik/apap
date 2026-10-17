//! Зеркало состояния движка в UI-потоке (ADR-02, И-Р13, И-Р14).
//!
//! Мост С3, шаг 25: события `EngineEvent` разбираются в UI-потоке и
//! накладываются на `UiAudioState`; в Slint-свойства пишутся только
//! изменившиеся поля (И-Р13) — у каждого поля есть копия «записано в UI»
//! (`Written<T>`). Устаревшие события отбрасываются по `req_gen` команды
//! открытия (И-Р14, ТЗ-56): модуль не знает о Slint и не пишет в UI —
//! подключение к `MusicApp`/`ui_manager.rs` происходит в шаге 28 (после
//! подключения мост снимается).
//!
//! Шаг 30: состояние хранит новую модель пути сигнала (§2.3, §2.5, ADR-20) —
//! `path: Option<Arc<SignalPath>>` (сравнение по `generation`, ТЗ-68),
//! `badge: BadgeState` (через `Written<T>`, И-Р13) и `DynamicPathState`
//! (§3.5) без обвязки `Written<T>`: событие `DynamicsChanged` появится
//! отдельным шагом, здесь поле только сбрасывается на новое открытие.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use music_player_rs::audio::backend::catalog::DeviceCatalog;
use music_player_rs::audio::decoder::TrackInfo;
use music_player_rs::audio::error::{OpenError, Reaction};
use music_player_rs::audio::path::signal::{DynamicPathState, SignalPath};
use music_player_rs::audio::player::StreamDesc;
use music_player_rs::audio::verdict::BadgeState;
use music_player_rs::engine::messages::{EngineEvent, Notice, SkipReason, TransportState};

/// Безопасное преобразование числа фреймов (`u64`) в `f64` для расчёта
/// длительности трека (ТЗ-103). `as` на сэмплах запрещён правилами проекта;
/// здесь это счётчик фреймов, не сэмпл, и потеря точности `u64 -> f64`
/// (мантисса 53 бита, ~9·10^15) недостижима для реальных треков — это
/// намного больше, чем число фреймов трека на разумной частоте дискретизации.
fn frames_to_f64(frames: u64) -> f64 {
    frames as f64
}

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

    /// Последнее принятое значение без снятия «грязного» флага (ADR-02,
    /// И-Р13) — для геттеров, читающих состояние многократно вне такта
    /// синхронизации с UI.
    fn get(&self) -> Option<&T> {
        self.value.as_ref()
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

/// `SignalPath` реализует `PartialEq`, но полное сравнение полей дороже и не
/// нужно (§3.1, ТЗ-68): новое открытие всегда даёт новый `generation`, тем
/// же самым путём движок повторно не присылает. Отсутствие пути (`None`,
/// пока путь не построен, или после неудачного открытия) — отдельное
/// значение снимка.
#[derive(Clone)]
struct PathSnapshot(Option<Arc<SignalPath>>);

impl PartialEq for PathSnapshot {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => a.generation == b.generation,
            (None, None) => true,
            _ => false,
        }
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
    path: Written<PathSnapshot>,
    badge: Written<BadgeState>,
    dynamics: DynamicPathState,
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
            EngineEvent::Opened { req_gen, info, stream, path, badge } => {
                if req_gen < self.latest_req_gen {
                    return Applied::None;
                }
                self.session = Some(req_gen);
                self.track.set(TrackSnapshot { session: req_gen, info: Arc::new(info) });
                self.stream.set(StreamSnapshot { session: req_gen, desc: Arc::new(stream) });
                self.position_secs.set(0.0);
                self.path.set(PathSnapshot(path));
                self.badge.set(badge);
                self.dynamics = DynamicPathState::default();
                Applied::None
            }
            EngineEvent::Skipped { req_gen, path, reason } => {
                if req_gen < self.latest_req_gen {
                    return Applied::None;
                }
                // ТЗ-56: параметры прошлого трека не переносятся на пропуск.
                self.session = None;
                self.path.set(PathSnapshot(None));
                self.badge.set(BadgeState::NoStream);
                self.dynamics = DynamicPathState::default();
                Applied::Skipped { path, reason }
            }
            EngineEvent::OpenFailed { req_gen, err } => {
                if req_gen < self.latest_req_gen {
                    return Applied::None;
                }
                // ТЗ-56: параметры прошлого трека не переносятся на неудачу.
                self.session = None;
                self.path.set(PathSnapshot(None));
                self.badge.set(BadgeState::NoStream);
                self.dynamics = DynamicPathState::default();
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
            EngineEvent::BadgeChanged(badge) => {
                // Тот же фильтр И-Р14, что и у `Position`: бейдж пересчитан
                // после перестройки пути для текущего трека (громкость/mute,
                // ТЗ-38, ТЗ-60), поэтому относится только к активной сессии
                // последнего запрошенного открытия.
                if self.session != Some(self.latest_req_gen) {
                    return Applied::None;
                }
                self.badge.set(badge);
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

    /// `Some`, только если бейдж изменился с прошлого чтения (И-Р13, ADR-20).
    pub fn take_badge_dirty(&mut self) -> Option<BadgeState> {
        self.badge.take_dirty().cloned()
    }

    /// `Some`, только если путь сменил `generation` (ТЗ-68), включая переход
    /// к `None` (путь не построен или открытие не удалось, ТЗ-56).
    pub fn take_path_dirty(&mut self) -> Option<Option<Arc<SignalPath>>> {
        self.path.take_dirty().map(|p| p.0.clone())
    }

    /// Текущий путь сигнала без снятия «грязного» флага (для чтения вне
    /// такта синхронизации, например статус-строкой).
    pub fn path(&self) -> Option<&Arc<SignalPath>> {
        self.path.value.as_ref().and_then(|p| p.0.as_ref())
    }

    /// Текущая динамическая часть пути (§3.5). Событие `DynamicsChanged` и
    /// грязный флаг для него — отдельный шаг; пока поле только сбрасывается
    /// на новое открытие и читается целиком.
    pub fn dynamics(&self) -> &DynamicPathState {
        &self.dynamics
    }

    /// Последнее принятое транспортное состояние без снятия «грязного»
    /// флага (ADR-02, И-Р13) — для многократного чтения вне такта
    /// синхронизации с UI.
    pub fn transport(&self) -> Option<TransportState> {
        self.transport.get().copied()
    }

    /// Последняя принятая позиция воспроизведения, `0.0` до первого
    /// принятого `Opened` (ADR-02, И-Р13).
    pub fn position_secs(&self) -> f64 {
        self.position_secs.get().copied().unwrap_or(0.0)
    }

    /// Метаданные активного трека последней принятой сессии открытия
    /// (ADR-02, И-Р13).
    pub fn track(&self) -> Option<&Arc<TrackInfo>> {
        self.track.get().map(|s| &s.info)
    }

    /// Описание потока вывода последней принятой сессии открытия; `None`,
    /// если поток не открыт (ADR-02, И-Р13).
    pub fn stream_desc(&self) -> Option<&StreamDesc> {
        self.stream.get().and_then(|s| s.desc.as_ref().as_ref())
    }

    /// Истина, если транспорт воспроизводит (ADR-02, И-Р13).
    pub fn is_playing(&self) -> bool {
        self.transport() == Some(TransportState::Playing)
    }

    /// Истина, если транспорт остановлен (§6.18, ТЗ-20): отсутствие
    /// принятого `Transport` (трек ещё не открывался) трактуется как
    /// остановленное состояние — то же правило, что и у моста С3.
    pub fn is_stopped(&self) -> bool {
        matches!(self.transport(), None | Some(TransportState::Stopped))
    }

    /// Длительность активного трека в секундах, если известно число
    /// фреймов (ТЗ-103, ADR-02).
    pub fn duration(&self) -> Option<f64> {
        let info = self.track()?;
        let frames = info.num_frames?;
        Some(frames_to_f64(frames) / f64::from(info.sample_rate))
    }

    /// `(частота, число каналов)` последнего принятого потока вывода, либо
    /// дефолт до первого открытия (ADR-02, И-Р13).
    pub fn format(&self) -> (u32, usize) {
        match self.stream_desc() {
            Some(desc) => (desc.rate, usize::from(desc.channels)),
            None => (44_100, 2),
        }
    }

    /// Имя устройства последнего принятого потока вывода, `"none"`, если
    /// поток не открыт (ADR-02, И-Р13).
    pub fn device_desc(&self) -> &str {
        match self.stream_desc() {
            Some(desc) => &desc.device,
            None => "none",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use music_player_rs::audio::decoder::Tags;
    use music_player_rs::audio::error::FileError;
    use music_player_rs::audio::verdict::{NbpReason, NbpSummary, SharedBadge};
    use music_player_rs::engine::messages::SkipReason;

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
        EngineEvent::Opened {
            req_gen,
            info: track_info(),
            stream: Some(StreamDesc::default()),
            path: None,
            badge: BadgeState::NoStream,
        }
    }

    /// Снимок `SignalPath` для тестов (Shared/PCM, без преобразований) —
    /// только то, что нужно, чтобы отличать поколения (ТЗ-68).
    fn test_signal_path(generation: u64) -> Arc<SignalPath> {
        use std::num::NonZeroU8;

        use music_player_rs::audio::backend::{AccessKind, DriverFormat, OutputApi};
        use music_player_rs::audio::format::{
            BitDepth, ChannelLayout, Codec, Container, OutputSampleFormat, SampleRate, SourceFormat, SourceKind,
            StreamFormat,
        };
        use music_player_rs::audio::path::{OpenedFacts, OsStage, PathPlan, ReservationView, Route};
        use music_player_rs::audio::session::RingPayload;
        use music_player_rs::settings::playback::ModeKind;
        use smallvec::SmallVec;

        let rate = SampleRate::new(44_100).expect("44100 — валидная частота");
        let bits = BitDepth::new(16).expect("16 бит — валидная разрядность");
        let target = StreamFormat {
            rate,
            sample: OutputSampleFormat::S32Le,
            channels: NonZeroU8::new(2).expect("2 канала — валидно"),
        };
        let plan = PathPlan {
            mode: ModeKind::Optimal,
            source: SourceFormat {
                container: Container::Flac,
                codec: Codec::Flac,
                lossy: false,
                rate,
                layout: ChannelLayout::stereo(),
                kind: SourceKind::Pcm { bits },
            },
            route: Route::Pcm,
            transforms: SmallVec::new(),
            target,
            ring: RingPayload::ExactI32 { valid_bits: bits },
            dsd_chain: None,
        };
        let facts = OpenedFacts {
            access: AccessKind::Shared,
            api: OutputApi::PipeWire,
            device_name: "test".to_owned(),
            driver: DriverFormat::Confirmed(target),
            reservation: ReservationView::NotApplicable,
            os: OsStage::NotInvolved,
            device_buffer: None,
        };
        Arc::new(SignalPath::from_opened(plan, facts, generation))
    }

    fn opened_with_badge(req_gen: u64, path: Option<Arc<SignalPath>>, badge: BadgeState) -> EngineEvent {
        EngineEvent::Opened { req_gen, info: track_info(), stream: Some(StreamDesc::default()), path, badge }
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

    /// ТЗ-56, И-Р14: неудачное открытие следующего трека сбрасывает бейдж и
    /// путь прошлого трека — параметры прошлого трека не переносятся.
    #[test]
    fn badge_no_stream_after_failed_open() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened_with_badge(
            1,
            Some(test_signal_path(1)),
            BadgeState::Shared(SharedBadge::Clean),
        ));
        assert!(state.path().is_some());

        state.begin_open(2);
        state.apply(EngineEvent::OpenFailed { req_gen: 2, err: OpenError::DeviceLost });

        assert_eq!(state.take_badge_dirty(), Some(BadgeState::NoStream));
        assert!(state.path().is_none());
    }

    /// ТЗ-60, И-Р13: тики транспорта/позиции не трогают бейдж — ни одной
    /// лишней записи, пока бейдж не меняется движком.
    #[test]
    fn badge_no_recompute_on_idle_ticks() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened_with_badge(1, None, BadgeState::Shared(SharedBadge::Clean)));
        assert!(state.take_badge_dirty().is_some());

        let mut writes = 0;
        for _ in 0..100 {
            state.apply(EngineEvent::Transport { state: TransportState::Playing });
            state.apply(EngineEvent::Position { secs: 1.0 });
            if state.take_badge_dirty().is_some() {
                writes += 1;
            }
        }
        assert_eq!(writes, 0);
    }

    /// ТЗ-38, ТЗ-139: пересчёт бейджа на mute/снятии mute без переоткрытия
    /// потока (та же сессия); устаревший `BadgeChanged` до принятого
    /// `Opened` отбрасывается (И-Р14).
    #[test]
    fn badge_shows_mute_and_restores_without_reopen() {
        let mut state = UiAudioState::new();

        // До какого-либо принятого Opened сессии нет — BadgeChanged отбрасывается.
        let dropped = state.apply(EngineEvent::BadgeChanged(BadgeState::Shared(SharedBadge::Clean)));
        assert_eq!(dropped, Applied::None);
        assert!(state.take_badge_dirty().is_none());

        state.begin_open(1);
        state.apply(opened_with_badge(1, None, BadgeState::Shared(SharedBadge::Clean)));
        assert_eq!(state.take_badge_dirty(), Some(BadgeState::Shared(SharedBadge::Clean)));

        let muted = BadgeState::Shared(SharedBadge::NotBitPerfect(NbpSummary { primary: NbpReason::Mute, more: 0 }));
        state.apply(EngineEvent::BadgeChanged(muted.clone()));
        assert_eq!(state.take_badge_dirty(), Some(muted));

        state.apply(EngineEvent::BadgeChanged(BadgeState::Shared(SharedBadge::Clean)));
        assert_eq!(state.take_badge_dirty(), Some(BadgeState::Shared(SharedBadge::Clean)));

        // Сессия не менялась — восстановление без нового Opened.
        assert_eq!(state.session, Some(1));
    }

    /// ТЗ-68: путь становится «грязным» только при смене `generation`,
    /// включая переход к `None` (открытие не удалось).
    #[test]
    fn path_dirty_only_on_generation_change() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened_with_badge(1, Some(test_signal_path(1)), BadgeState::NoStream));
        assert!(state.take_path_dirty().is_some());

        // Тот же generation — BadgeChanged его не трогает, take_path_dirty молчит.
        state.apply(EngineEvent::BadgeChanged(BadgeState::Shared(SharedBadge::Clean)));
        assert!(state.take_path_dirty().is_none());

        state.begin_open(2);
        state.apply(EngineEvent::OpenFailed { req_gen: 2, err: OpenError::DeviceLost });
        assert_eq!(state.take_path_dirty(), Some(None));
    }

    /// Геттеры читают последнее принятое значение (ADR-02, И-Р13) без
    /// привязки к разовому чтению `take_*_dirty`.
    #[test]
    fn getters_hold_last_accepted_values() {
        let mut state = UiAudioState::new();
        assert!(state.track().is_none());
        assert_eq!(state.format(), (44_100, 2));
        assert_eq!(state.device_desc(), "none");

        state.begin_open(1);
        state.apply(opened(1));
        state.apply(EngineEvent::Transport { state: TransportState::Playing });
        state.apply(EngineEvent::Position { secs: 7.5 });

        assert_eq!(state.transport(), Some(TransportState::Playing));
        assert_eq!(state.position_secs(), 7.5);
        assert!(state.track().is_some());
        assert!(state.is_playing());
        assert!(!state.is_stopped());
        assert_eq!(state.duration(), Some(1_000.0 / 44_100.0));
    }

    /// `take_*_dirty` снимает «грязный» флаг, но не меняет значение,
    /// отдаваемое геттерами (ADR-02, И-Р13).
    #[test]
    fn getters_survive_take_dirty() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened(1));
        state.apply(EngineEvent::Transport { state: TransportState::Playing });

        assert!(state.take_track_dirty().is_some());
        assert!(state.take_transport_dirty().is_some());

        assert!(state.track().is_some());
        assert_eq!(state.transport(), Some(TransportState::Playing));
    }

    /// Устаревшее событие (И-Р14) не меняет значение, видимое геттерами.
    #[test]
    fn stale_event_does_not_change_getters() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened(1));
        let track_before = state.track().cloned();

        state.begin_open(2);
        state.apply(opened(1));

        let track_after = state.track().cloned();
        assert!(Arc::ptr_eq(track_before.as_ref().expect("трек открыт"), track_after.as_ref().expect("трек открыт")));
    }

    /// Формат и имя устройства до первого открытия — дефолт, повторяющий
    /// поведение моста С3.
    #[test]
    fn format_and_device_default_before_open() {
        let state = UiAudioState::new();
        assert_eq!(state.format(), (44_100, 2));
        assert_eq!(state.device_desc(), "none");
        assert!(state.stream_desc().is_none());
        assert!(state.is_stopped());
        assert!(!state.is_playing());
    }
}
