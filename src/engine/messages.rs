//! Команды и события потока apap-engine (§2.8, ADR-01). Мост С3: подмножество
//! сообщений, нужное, пока `Player` живёт внутри движка целиком — без
//! `SignalPath`/`BadgeState`/`DeviceCatalog` (приходят на С4/С5/следующих шагах).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::Path;
use std::sync::Arc;

use crate::audio::backend::catalog::DeviceCatalog;
use crate::audio::backend::BackendError;
use crate::audio::decoder::TrackInfo;
use crate::audio::error::{FileError, OpenError, Reaction};
use crate::audio::player::{ReservationEvent, StreamDesc};
use crate::settings::params::ModeSettingsUpdate;
use crate::settings::playback::ModeKind;
use crate::settings::{
    ClockFamily, DsdMode, ExclusiveMode, FallbackPolicy, FallbackRatePolicy, ResamplerAlgorithm,
    ResamplerDither, ResamplerMode,
};

/// Старые параметры звука, применяемые в памяти движка без I/O (ТЗ-134, И-Р24).
/// Мост С3: больше не приходит командой от UI — строится внутри движка
/// (`legacy_audio` в `legacy_path.rs`) из `ModeSettings` при `SetModeSettings`/
/// `SetActiveMode` (ОВС-14, ОВС-17, §6.18, ADR-22).
#[derive(Clone, PartialEq, Debug)]
pub struct LegacyAudio {
    pub exclusive_mode: ExclusiveMode,
    pub fallback_policy: FallbackPolicy,
    pub dsd_mode: DsdMode,
    pub resampler_mode: ResamplerMode,
    pub resampler_algorithm: ResamplerAlgorithm,
    pub fixed_rate: u32,
    pub prefer_family: ClockFamily,
    pub fallback_rate: FallbackRatePolicy,
    pub ring_buffer_ms: u32,
    pub bit_perfect: bool,
    pub dither: ResamplerDither,
}

/// Команды UI → движок (мост С3).
pub enum EngineCmd {
    Open { req_gen: u64, path: Arc<Path>, start_secs: f64, autoplay: bool },
    Play,
    Pause,
    Stop,
    /// Мост: секунды, в С5 — frame.
    Seek { secs: f64 },
    SetVolume(f32),
    SetMuted(bool),
    /// Выбор устройства вывода; позиция/трек переоткрытия — у движка.
    SetDevice { id: String },
    RefreshDevices,
    /// Включение tap визуализатора (§2.8).
    SetVizTap(bool),
    /// Мост С3: передача producer'а tap в движок (сейчас `Player::set_viz_tap`).
    AttachVizTap(Option<rtrb::Producer<f32>>),
    /// Новые настройки режима (ОВС-14, ОВС-17, §6.18, ADR-22): движок хранит
    /// копию `settings`, а по `diff.strongest(active)` переоткрывает поток на
    /// текущей позиции (`ApplyKind::ReopenAtPosition`/`SwitchDevice`) или
    /// только обновляет копию (`None`/`Memory`). `diff: None` — всегда копия.
    SetModeSettings(ModeSettingsUpdate),
    /// Переключение активного режима (ТЗ-20, ОВ-10, §6.18): переоткрытие на
    /// текущей позиции с параметрами нового режима (как `SwitchDevice`),
    /// т.к. устройство/эксклюзивность/bit-perfect могут отличаться.
    SetActiveMode(ModeKind),
    /// Мост С3: освободить монопольный узел `hw:` без перемотки (конец
    /// плейлиста, уход в трей — V5.1-B5/B6). В С6 заменяется `release`
    /// монопольного бэкенда (ТЗ-48).
    ReleaseExclusive,
    Shutdown,
}

impl std::fmt::Debug for EngineCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineCmd::Open { req_gen, path, start_secs, autoplay } => f
                .debug_struct("Open")
                .field("req_gen", req_gen)
                .field("path", path)
                .field("start_secs", start_secs)
                .field("autoplay", autoplay)
                .finish(),
            EngineCmd::Play => write!(f, "Play"),
            EngineCmd::Pause => write!(f, "Pause"),
            EngineCmd::Stop => write!(f, "Stop"),
            EngineCmd::Seek { secs } => f.debug_struct("Seek").field("secs", secs).finish(),
            EngineCmd::SetVolume(v) => f.debug_tuple("SetVolume").field(v).finish(),
            EngineCmd::SetMuted(m) => f.debug_tuple("SetMuted").field(m).finish(),
            EngineCmd::SetDevice { id } => f.debug_struct("SetDevice").field("id", id).finish(),
            EngineCmd::RefreshDevices => write!(f, "RefreshDevices"),
            EngineCmd::SetVizTap(on) => f.debug_tuple("SetVizTap").field(on).finish(),
            EngineCmd::AttachVizTap(producer) => {
                f.debug_tuple("AttachVizTap").field(&producer.as_ref().map(|_| "<producer>")).finish()
            }
            EngineCmd::SetModeSettings(update) => {
                f.debug_tuple("SetModeSettings").field(update).finish()
            }
            EngineCmd::SetActiveMode(mode) => f.debug_tuple("SetActiveMode").field(mode).finish(),
            EngineCmd::ReleaseExclusive => write!(f, "ReleaseExclusive"),
            EngineCmd::Shutdown => write!(f, "Shutdown"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportState {
    Stopped,
    Playing,
    Paused,
}

/// Incompatible(Blocked) — С5.
#[derive(Clone, PartialEq, Debug)]
pub enum SkipReason {
    File(FileError),
}

/// Уведомления движка (подмножество С3, ТЗ-102).
#[derive(Clone, PartialEq, Debug)]
pub enum Notice {
    Reservation(ReservationEvent),
    /// Перечисление устройств упало — хост не ответил (ТЗ-104, ТЗ-105, ADR-16, §6.2 п.3).
    DevicesUnavailable(BackendError),
}

/// События движок → UI (мост С3).
// `Opened` несёт `TrackInfo` целиком только на мосту С3 (replaced by
// `SignalPath`/`BadgeState` на С5); boxing здесь — преждевременная
// оптимизация до стабилизации формы события.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// Мост С3: `TrackInfo` и `StreamDesc` старого пути вместо `SignalPath`/`BadgeState` (С5).
    Opened { req_gen: u64, info: TrackInfo, stream: Option<StreamDesc> },
    Skipped { req_gen: u64, path: Arc<Path>, reason: SkipReason },
    OpenFailed { req_gen: u64, err: OpenError },
    /// Мост С3: session = req_gen открытия.
    Ended { session: u64 },
    Transport { state: TransportState },
    /// Только из цикла движка, при изменении (И-Р13); мост: секунды.
    Position { secs: f64 },
    DeviceLost { reaction: Reaction },
    /// Каталог устройств после перечисления (ТЗ-104, ТЗ-105, ADR-16, §6.2 п.3):
    /// шлётся при старте движка и затем только при изменении списка.
    Devices(Arc<DeviceCatalog>),
    Notice(Notice),
    ShutdownComplete,
}

#[allow(dead_code)]
const _: () = {
    fn assert_send<T: Send>() {}
    fn check() {
        assert_send::<EngineCmd>();
        assert_send::<EngineEvent>();
    }
};
