//! Команды и события потока apap-engine (§2.8, ADR-01). Мост С3: подмножество
//! сообщений, нужное, пока `Player` живёт внутри движка целиком — без
//! `SignalPath`/`BadgeState`/`DeviceCatalog` (приходят на С4/С5/следующих шагах).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::Path;
use std::sync::Arc;

use crate::audio::backend::catalog::DeviceCatalog;
use crate::audio::backend::exclusive::DeviceClaim;
use crate::audio::backend::{BackendError, DeviceCaps, HwDeviceId};
use crate::audio::decoder::TrackInfo;
use crate::audio::error::{CaptureFailure, FileError, OpenError, Reaction, StreamFault};
use crate::audio::format::StreamFormat;
use crate::audio::path::signal::{DynamicPathState, SignalPath};
use crate::audio::path::Blocked;
use crate::audio::verdict::BadgeState;
use crate::engine::lock::{LockInput, LockState};
use crate::playlist::model::TrackId;
use crate::settings::params::ModeSettingsUpdate;
use crate::settings::playback::ModeKind;
use crate::settings::{
    ClockFamily, DsdMode, FallbackPolicy, FallbackRatePolicy, ResamplerAlgorithm,
    ResamplerDither, ResamplerMode,
};

/// Ссылка на трек без тяжёлого `Track`: id строки плейлиста и путь (§2.8).
/// Точка повторного захвата замка хранит её вместо пути (§2.6). `id` —
/// `None` на мосту С3 (ТЗ-126, §6.20): `EngineCmd::Open` и `self.current`
/// движка несут только путь, без `TrackId` строки плейлиста (`Playlist`
/// выдаёт его только себе, `alloc_id` приватен); станет `Some` с С8+, когда
/// `TrackId` дойдёт до движка. Повторный захват (§6.20 п. 4) открывает трек
/// по пути — `id` не требуется для самой перемотки.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrackRef {
    pub id: Option<TrackId>,
    pub path: Arc<Path>,
}

/// Старые параметры звука Совместимого режима, применяемые в памяти движка
/// без I/O (ТЗ-134, И-Р24, §8 С6). Мост С3: больше не приходит командой от
/// UI — строится внутри движка (`legacy_audio` в `legacy_path.rs`) из
/// `CompatibleOpts` при `SetModeSettings`/`SetActiveMode` (ОВС-14, ОВС-17,
/// §6.18, ADR-22). Оптимальный и Строгий режимы старый путь не используют —
/// их вывод ведёт `AlsaExclusiveBackend` (ADR-24).
#[derive(Clone, PartialEq, Debug)]
pub struct LegacyAudio {
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
    /// Команда замка устройства режима из главного окна (§2.8, §6.19).
    Lock(LockInput),
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
            EngineCmd::Lock(input) => f.debug_tuple("Lock").field(input).finish(),
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

// `Incompatible(Blocked)` несёт план целиком (форма задана спецификацией
// §2.4) — боксинг одного варианта расходится с объявленной сигнатурой enum.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Debug)]
pub enum SkipReason {
    /// Трек несовместим с устройством в Строгом режиме: план остановлен на
    /// этапе `blocked.at` (§2.4, ТЗ-57, ТЗ-80).
    Incompatible(Blocked),
    File(FileError),
}

/// Уведомления движка (подмножество С3, ТЗ-102).
#[derive(Clone, PartialEq, Debug)]
pub enum Notice {
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
    /// `path`/`badge` — новая модель (§2.3, §2.5, ADR-20): `None`/
    /// `BadgeState::NoStream`, пока движок не построил путь (например,
    /// Оптимальный/Строгий на легаси-исполнителе). `stream_format` — формат
    /// вывода старого пути для всех режимов (С5-мост, ADR-05, §2.3): поле
    /// снимается на этапе С6, когда `SignalPath` строится во всех режимах.
    Opened {
        req_gen: u64,
        info: TrackInfo,
        stream_format: Option<StreamFormat>,
        path: Option<Arc<SignalPath>>,
        badge: BadgeState,
    },
    Skipped { req_gen: u64, path: Arc<Path>, reason: SkipReason },
    OpenFailed { req_gen: u64, err: OpenError },
    /// Мост С3: session = req_gen открытия.
    Ended { session: u64 },
    Transport { state: TransportState },
    /// Только из цикла движка, при изменении (И-Р13); мост: секунды.
    Position { secs: f64 },
    /// Бейдж пересчитан после перестройки пути (громкость/mute) и отправлен
    /// только при изменении вердикта (ТЗ-60, §4.3).
    BadgeChanged(BadgeState),
    /// Состояние замка устройства режима (§2.8, §6.19, ТЗ-118).
    Lock(LockState),
    /// Динамическая часть пути сигнала (underruns/xruns/RT-приоритет/MD5/…),
    /// шлётся только при изменении относительно последней отправленной копии
    /// (ТЗ-68, §6.18 п.3, И-Р13). `session` — `req_gen` открытого трека, как
    /// у `Ended`/`OpenFailed` (мост С3: отдельного поколения Эксклюзивного
    /// пути в событиях пока нет).
    DynamicsChanged { session: u64, dynamics: DynamicPathState },
    /// `fault` — код `StreamFault`, который привёл к потере устройства на
    /// Эксклюзивном пути (§6.20, ТЗ-51, ТЗ-76, AM1.0-B34): `None` для потерь
    /// без `StreamFault` (например, `ReservationLost` — потеря имени
    /// `ReserveDevice1`). `Some(StreamFault::Stalled)` — особый случай:
    /// поток открыт, но hw-указатель никогда не двинулся (сторож запуска,
    /// `alsa_out.rs::run_output_loop`) — приложение показывает отдельное
    /// сообщение «Устройство не запустило воспроизведение» вместо общего
    /// «Устройство вывода потеряно» (`app/mod.rs::Applied::DeviceLost`).
    /// Если в будущем `StreamFault::Stalled` станет также означать
    /// мид-трековый обрыв (не только недостигнутый старт), этот вариант
    /// нужно развести на два отдельных кода `StreamFault`, иначе здесь
    /// снова смешаются два разных по смыслу случая.
    DeviceLost { reaction: Reaction, fault: Option<StreamFault> },
    /// Каталог устройств после перечисления (ТЗ-104, ТЗ-105, ADR-16, §6.2 п.3):
    /// шлётся при старте движка и затем только при изменении списка.
    Devices(Arc<DeviceCatalog>),
    /// Возможности аппаратного устройства, снятые при его открытии
    /// (AM1.0-B30, решение 2026-10-24): шлётся из `open_exclusive` сразу
    /// после `ExclusiveBackend::device_caps`, в дополнение к `Opened` — не
    /// только при изменении каталога, в отличие от `Devices`. UI кэширует
    /// по `device` (`HwDeviceId`), без повторного открытия, для блока
    /// «Аудио» диалога настроек (вкладка показывает возможности устройства,
    /// выбранного в редактируемом режиме, а не только фактически играющего).
    DeviceCapsCached { device: HwDeviceId, caps: Arc<DeviceCaps> },
    /// Автовыбор устройства режима (ТЗ-143, ADR-24); приложение сохраняет его в настройках.
    DeviceAutoSelected { mode: ModeKind, device: HwDeviceId },
    Notice(Notice),
    ShutdownComplete,
}

/// Сообщения служебных потоков движку (§2.8, ADR-01). Каждое несёт поколение
/// попытки (`attempt`/`session`), чтобы движок отбрасывал ответы на
/// отменённые операции.
pub enum InternalMsg {
    /// Имя `ReserveDevice1` потеряно (сигнал `NameLost`, ADR-08, И-Р20).
    ReservationLost { card: u32 },
    /// Итог открытия PCM после резервирования — поступает служебным потоком
    /// захвата; потребление замком (`lock_step`) приходит с его проводкой
    /// (§6.19, шаги 34/52/56).
    ClaimDone { attempt: u64, result: Result<DeviceClaim, CaptureFailure> },
    /// Итог RT-приоритета (§6.8): на поведение движка не влияет.
    RtPriority { session: u64, granted: Option<u32> },
}

impl std::fmt::Debug for InternalMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InternalMsg::ReservationLost { card } => {
                f.debug_struct("ReservationLost").field("card", card).finish()
            }
            InternalMsg::ClaimDone { attempt, result } => f
                .debug_struct("ClaimDone")
                .field("attempt", attempt)
                .field("result", &result.as_ref().map(|_| "<claim>"))
                .finish(),
            InternalMsg::RtPriority { session, granted } => f
                .debug_struct("RtPriority")
                .field("session", session)
                .field("granted", granted)
                .finish(),
        }
    }
}

#[allow(dead_code)]
const _: () = {
    fn assert_send<T: Send>() {}
    fn check() {
        assert_send::<EngineCmd>();
        assert_send::<EngineEvent>();
        assert_send::<InternalMsg>();
    }
};
