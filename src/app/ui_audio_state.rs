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
//! (§3.5), сбрасываемое на новое открытие.
//!
//! Шаг 78 (ТЗ-68, §3.5, AM1.0-B8): `EngineEvent::DynamicsChanged` принимается
//! по тому же фильтру `session`, что и `Ended` — устаревшая сессия
//! отбрасывается (И-Р14). Грязный флаг для `dynamics` ведётся отдельной
//! копией «записано в UI» (`dynamics_written`), а не обвязкой `Written<T>`:
//! `dynamics()` обязан возвращать `&DynamicPathState` напрямую (читает
//! `path_view.rs::build_report`), а `Written<T>` отдаёт только `Option<&T>`.
//!
//! §8 С5: устаревший снимок старого пути (слот `stream`, геттер
//! `stream_desc()`) удалён из состояния UI. `format()` (§3.1, ADR-05) читает
//! `path().output.requested`, если путь построен (Совместимый режим), иначе
//! падает на принятый `stream_format` из `Opened` — формат старого пути,
//! единственный источник до С6, когда Оптимальный/Строгий режимы ещё не
//! строят `SignalPath` (С5-мост, §2.3).
//!
//! Шаг 72 (ТЗ-119, баг AM1.0-B9): `lock_acquiring` — единственное, что от
//! `EngineEvent::Lock` попадало в это состояние; полный вид замка остаётся
//! вне `UiAudioState` и строится отдельно в `apply_lock_view`.
//!
//! Шаг 74 (ТЗ-125, §6.19): UI-потоку нужен не только флаг «идёт захват»,
//! но и сам принятый `LockState` — чтобы решить, что делает Play при
//! открытом замке (шаг 75). Поле `lock` хранит последнее принятое
//! состояние целиком; `lock_acquiring()` сведён к `matches!` по нему.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use music_player_rs::audio::backend::catalog::DeviceCatalog;
use music_player_rs::audio::backend::HwDeviceId;
use music_player_rs::audio::decoder::TrackInfo;
use music_player_rs::audio::error::{OpenError, Reaction};
use music_player_rs::audio::format::StreamFormat;
use music_player_rs::audio::path::signal::{DynamicPathState, SignalPath};
use music_player_rs::audio::path::Blocked;
use music_player_rs::audio::player::ReservationEvent;
use music_player_rs::audio::verdict::BadgeState;
use music_player_rs::engine::lock::LockState;
use music_player_rs::engine::messages::{EngineEvent, Notice, SkipReason, TransportState};
use music_player_rs::settings::playback::ModeKind;

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
    /// Автовыбор устройства Оптимального/Строгого режима (ТЗ-143, ТЗ-134,
    /// ADR-24) — сохраняется в настройках вызывающей стороной (шаг 38).
    DeviceAutoSelected { mode: ModeKind, device: HwDeviceId },
    Notice(Notice),
    ShutdownComplete,
}

/// Зеркало состояния движка в UI-потоке (ADR-02).
///
/// `begin_open` вызывается UI при отправке `EngineCmd::Open` — до ответа
/// движка — и фиксирует новый `req_gen` как «последний запрошенный»: любое
/// `Opened`/`Skipped`/`OpenFailed` с меньшим `req_gen` относится к уже
/// переоткрытому треку и отбрасывается (И-Р14, ТЗ-56).
pub struct UiAudioState {
    latest_req_gen: u64,
    /// `req_gen` последнего принятого `Opened` (= id активной сессии).
    session: Option<u64>,
    transport: Written<TransportState>,
    position_secs: Written<f64>,
    track: Written<TrackSnapshot>,
    devices: Written<DeviceSnapshot>,
    path: Written<PathSnapshot>,
    /// Формат вывода старого пути принятой сессии (С5-мост, §2.3, ADR-05) —
    /// запасной источник `format()`, пока `path()` не построен (Оптимальный/
    /// Строгий). Те же правила принятия/сброса, что и у `path`.
    stream_format: Option<StreamFormat>,
    badge: Written<BadgeState>,
    dynamics: DynamicPathState,
    /// Копия `dynamics`, «записанная в UI» (ТЗ-68, §3.5, шаг 78) — тот же
    /// смысл, что и `written` в `Written<T>`, но без обвязки generic:
    /// `dynamics()` должен возвращать `&DynamicPathState`, а не `Option`.
    // Читается только в `take_dynamics_dirty` — используется на шаге 79
    // (`path_view.rs`), вне теста пока не вызывается.
    #[allow(dead_code)]
    dynamics_written: Option<DynamicPathState>,
    /// Истина после принятого `Ended`, пока не снята `clear_end` (ТЗ-56,
    /// ADR-02).
    ended: bool,
    /// Вновь принятый `Opened`, ещё не забранный вызывающей стороной —
    /// снимается один раз через `take_opened` (И-Р14, ADR-02).
    opened: Option<Arc<TrackInfo>>,
    /// Уведомления о резервировании устройства, ожидающие снятия, FIFO
    /// (ТЗ-56, ADR-02) — терминальные `Notice::Reservation` движок шлёт без
    /// фильтра по `req_gen`, тем же правилом, что и мост С3.
    reservation_queue: VecDeque<ReservationEvent>,
    /// Активный план пропуска (`SkipReason::Incompatible`, §2.4, ТЗ-80) —
    /// предмет отчёта окна «Путь сигнала» (`PathSubject::Skipped`), пока не
    /// снят новым успешно открытым путём или иным исходом открытия (ТЗ-79).
    skipped: Option<Blocked>,
    /// Последнее принятое состояние замка устройства (ТЗ-119, ТЗ-125,
    /// ADR-02, §6.19), целиком — шаг 74; `NotApplicable` до первого
    /// принятого `EngineEvent::Lock` (Совместимый режим, ТЗ-50). Полный вид
    /// замка (`lock-kind`/`lock-text`/…) строится отдельно из сырого
    /// `EngineEvent::Lock` в `apply_lock_view`.
    lock: LockState,
}

impl Default for UiAudioState {
    /// `LockState` не реализует `Default` (ADR-20) — остальные поля
    /// повторяют значения, которые раньше давал `#[derive(Default)]`.
    fn default() -> Self {
        Self {
            latest_req_gen: 0,
            session: None,
            transport: Written::default(),
            position_secs: Written::default(),
            track: Written::default(),
            devices: Written::default(),
            path: Written::default(),
            stream_format: None,
            badge: Written::default(),
            dynamics: DynamicPathState::default(),
            dynamics_written: None,
            ended: false,
            opened: None,
            reservation_queue: VecDeque::new(),
            skipped: None,
            lock: LockState::NotApplicable,
        }
    }
}

impl UiAudioState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Регистрирует `req_gen` команды `Open`, отправленной UI (И-Р14), и
    /// сбрасывает флаг завершения прошлой сессии (ТЗ-56, ADR-02).
    pub fn begin_open(&mut self, req_gen: u64) {
        self.latest_req_gen = req_gen;
        self.ended = false;
    }

    /// Накладывает событие движка на состояние. Возвращает `Applied::None`
    /// для событий, которые полностью свелись к полю (или были отброшены
    /// как устаревшие) — значения читаются через `take_*_dirty`.
    pub fn apply(&mut self, ev: EngineEvent) -> Applied {
        match ev {
            EngineEvent::Opened { req_gen, info, stream_format, path, badge } => {
                if req_gen < self.latest_req_gen {
                    return Applied::None;
                }
                self.session = Some(req_gen);
                let info = Arc::new(info);
                self.track.set(TrackSnapshot { session: req_gen, info: info.clone() });
                // Снимается один раз через `take_opened` (И-Р14, ADR-02).
                self.opened = Some(info);
                self.position_secs.set(0.0);
                // Успешно открытый путь снимает хранимый план пропуска — он
                // относится к прошлому треку (ТЗ-79, ТЗ-80).
                if path.is_some() {
                    self.skipped = None;
                }
                self.path.set(PathSnapshot(path));
                self.stream_format = stream_format;
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
                self.stream_format = None;
                self.badge.set(BadgeState::NoStream);
                self.dynamics = DynamicPathState::default();
                // Только несовместимость (ТЗ-80) — предмет отчёта «Путь
                // сигнала»; прочие причины пропуска снимают прошлый план.
                self.skipped = match &reason {
                    SkipReason::Incompatible(blocked) => Some(blocked.clone()),
                    SkipReason::File(_) => None,
                };
                Applied::Skipped { path, reason }
            }
            EngineEvent::OpenFailed { req_gen, err } => {
                if req_gen < self.latest_req_gen {
                    return Applied::None;
                }
                // ТЗ-56: параметры прошлого трека не переносятся на неудачу.
                self.session = None;
                self.path.set(PathSnapshot(None));
                self.stream_format = None;
                self.badge.set(BadgeState::NoStream);
                self.dynamics = DynamicPathState::default();
                self.skipped = None;
                Applied::OpenFailed { err }
            }
            EngineEvent::Ended { session } => {
                if self.session != Some(session) {
                    return Applied::None;
                }
                self.ended = true;
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
            // Полный вид замка строится отдельно из сырого `EngineEvent::Lock`
            // в `apply_lock_view` (§2.6, §2.8, §6.19); здесь принятое
            // состояние сохраняется целиком для `lock()`/`lock_acquiring()`
            // (ТЗ-119, ТЗ-125, шаг 74).
            EngineEvent::Lock(state) => {
                self.lock = state;
                Applied::None
            }
            EngineEvent::DeviceLost { reaction } => Applied::DeviceLost(reaction),
            EngineEvent::Devices(catalog) => {
                self.devices.set(DeviceSnapshot(catalog));
                Applied::None
            }
            // Автовыбор сохраняется приложением в настройках (ТЗ-143,
            // ТЗ-134, ADR-24, шаг 38) — одним снимком `settings.toml`, без
            // `EngineCmd::SetModeSettings` (у движка уже есть это значение,
            // повторная отправка вызвала бы петлю повторного открытия).
            EngineEvent::DeviceAutoSelected { mode, device } => Applied::DeviceAutoSelected { mode, device },
            EngineEvent::Notice(notice) => {
                // Движок публикует только терминальные `Notice::Reservation`
                // без фильтра по `req_gen` (ТЗ-56, ADR-02) — та же очередь,
                // что и у моста С3.
                if let Notice::Reservation(event) = &notice {
                    self.reservation_queue.push_back(event.clone());
                }
                Applied::Notice(notice)
            }
            EngineEvent::ShutdownComplete => Applied::ShutdownComplete,
            // Тот же фильтр по `session`, что и у `Ended` (ТЗ-68, §3.5, шаг
            // 78): движок шлёт `DynamicsChanged` с `session` открытой сессии
            // (бридж), устаревшая (уже закрытая/переоткрытая) отбрасывается.
            EngineEvent::DynamicsChanged { session, dynamics } => {
                if self.session != Some(session) {
                    return Applied::None;
                }
                self.dynamics = dynamics;
                Applied::None
            }
        }
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

    /// Истина после принятого `Ended`, пока не снята `clear_end` (ТЗ-56,
    /// ADR-02).
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Снять флаг завершения сессии (ТЗ-56, ADR-02).
    pub fn clear_end(&mut self) {
        self.ended = false;
    }

    /// Снять вновь принятый `Opened`, если он есть, — одноразовое чтение для
    /// вызывающей стороны (И-Р14, ADR-02); повторный вызов до следующего
    /// `Opened` вернёт `None`.
    pub fn take_opened(&mut self) -> Option<Arc<TrackInfo>> {
        self.opened.take()
    }

    /// Снять очередное уведомление о резервировании устройства, FIFO (ТЗ-56,
    /// ADR-02).
    pub fn poll_reservation(&mut self) -> Option<ReservationEvent> {
        self.reservation_queue.pop_front()
    }

    /// Активный план пропуска (`SkipReason::Incompatible`, §2.4, ТЗ-80),
    /// если предмет отчёта окна «Путь сигнала» сейчас — пропущенный трек.
    pub fn skipped(&self) -> Option<&Blocked> {
        self.skipped.as_ref()
    }

    /// Последнее принятое состояние замка устройства (ТЗ-119, ТЗ-125, ADR-02,
    /// §6.19), целиком — шаг 74: решает, что делает Play при открытом замке.
    /// Потребитель — `lock_blocks_play` (`playback_manager.rs`, шаг 75):
    /// гейтинг Play под открытым замком (ТЗ-125).
    pub(super) fn lock(&self) -> &LockState {
        &self.lock
    }

    /// Истина, пока замок устройства в `LockState::Acquiring` (ТЗ-119,
    /// ADR-02, §6.19) — строка состояния показывает «Захват устройства…»,
    /// пока не снят следующим принятым `EngineEvent::Lock` (шаг 72).
    pub(super) fn lock_acquiring(&self) -> bool {
        matches!(self.lock, LockState::Acquiring { .. })
    }

    /// Текущая динамическая часть пути (§3.5) без снятия «грязного» флага —
    /// для чтения вне такта синхронизации с UI (`path_view.rs::build_report`).
    pub fn dynamics(&self) -> &DynamicPathState {
        &self.dynamics
    }

    /// `Some`, только если динамическая часть пути изменилась с прошлого
    /// чтения (ТЗ-68, §3.5, И-Р13, шаг 78/79) — тот же смысл, что и
    /// `Written::take_dirty`, но без Option-обвязки текущего значения.
    // Используется на шаге 79 (`path_view.rs`); вне теста пока не вызывается.
    #[allow(dead_code)]
    pub fn take_dynamics_dirty(&mut self) -> Option<DynamicPathState> {
        if Some(&self.dynamics) != self.dynamics_written.as_ref() {
            self.dynamics_written = Some(self.dynamics.clone());
            Some(self.dynamics.clone())
        } else {
            None
        }
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

    /// `(частота, число каналов)` формата вывода последней принятой сессии
    /// (§3.1, ADR-05): `output.requested` построенного пути (`path()`), иначе
    /// `stream_format` старого пути (С5-мост, §2.3 — Оптимальный/Строгий до
    /// С6), иначе дефолт до первого открытия/после неудачного открытия
    /// (ADR-02, И-Р13).
    pub fn format(&self) -> (u32, usize) {
        if let Some(path) = self.path() {
            return (path.output.requested.rate.hz(), usize::from(path.output.requested.channels.get()));
        }
        if let Some(format) = &self.stream_format {
            return (format.rate.hz(), usize::from(format.channels.get()));
        }
        (44_100, 2)
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
            stream_format: None,
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
        EngineEvent::Opened { req_gen, info: track_info(), stream_format: None, path, badge }
    }

    #[test]
    fn stale_opened_dropped_after_newer_begin_open() {
        let mut state = UiAudioState::new();

        state.begin_open(1);
        state.apply(opened(1));
        assert!(state.track().is_some());

        // UI запросило новое открытие — req_gen 1 теперь устарел.
        state.begin_open(2);
        let applied = state.apply(opened(1));
        assert_eq!(applied, Applied::None);
        // Устаревшее событие не меняет сессию.
        assert_eq!(state.session, Some(1));
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

    /// Шаг 38 (ТЗ-143, ТЗ-134, ADR-24): `DeviceAutoSelected` не сводится к
    /// полю состояния — вызывающая сторона (`MusicApp`) сохраняет его в
    /// настройках, без фильтра по `req_gen` (устройство общее для режима,
    /// не для одной сессии открытия).
    #[test]
    fn device_auto_selected_passed_through() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=USB,DEV=0".parse().expect("валидный HwDeviceId");

        let applied = state.apply(EngineEvent::DeviceAutoSelected { mode: ModeKind::Optimal, device: device.clone() });

        assert_eq!(applied, Applied::DeviceAutoSelected { mode: ModeKind::Optimal, device });
    }

    /// И-Р14: позиция для несуществующей сессии (до принятого `Opened`)
    /// отбрасывается; `Opened` сбрасывает позицию на 0.
    #[test]
    fn opened_resets_position() {
        let mut state = UiAudioState::new();
        state.begin_open(1);

        // До принятого Opened позиция для несуществующей сессии отбрасывается.
        state.apply(EngineEvent::Position { secs: 5.0 });
        assert_eq!(state.position_secs(), 0.0);

        state.apply(opened(1));
        assert_eq!(state.position_secs(), 0.0);

        state.apply(EngineEvent::Position { secs: 42.0 });
        assert_eq!(state.position_secs(), 42.0);
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

    /// ТЗ-68, §3.5: `DynamicsChanged` для активной сессии взводит грязный
    /// флаг один раз; повторный тот же `dynamics` его не взводит.
    #[test]
    fn dynamics_dirty_only_on_change() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened(1));
        // Первый принятый Opened взводит грязный флаг переходом из
        // «нет данных» (dynamics_written == None) к дефолту — то же
        // поведение, что у `path`/`badge` на первом Opened.
        assert_eq!(state.take_dynamics_dirty(), Some(DynamicPathState::default()));
        assert!(state.take_dynamics_dirty().is_none());

        let dynamics = DynamicPathState { underruns: 1, ..DynamicPathState::default() };
        state.apply(EngineEvent::DynamicsChanged { session: 1, dynamics: dynamics.clone() });
        assert_eq!(state.take_dynamics_dirty(), Some(dynamics.clone()));

        // То же значение — не грязное.
        assert!(state.take_dynamics_dirty().is_none());
        state.apply(EngineEvent::DynamicsChanged { session: 1, dynamics: dynamics.clone() });
        assert!(state.take_dynamics_dirty().is_none());
    }

    /// ТЗ-68, §3.5, И-Р14: `DynamicsChanged` с чужой (не активной) сессией
    /// отбрасывается — тот же фильтр, что и у `Ended`.
    #[test]
    fn dynamics_changed_for_stale_session_ignored() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened(1));
        // Снимаем грязный флаг первого Opened, чтобы изолировать проверку.
        assert!(state.take_dynamics_dirty().is_some());

        let dynamics = DynamicPathState { underruns: 7, ..DynamicPathState::default() };
        let applied = state.apply(EngineEvent::DynamicsChanged { session: 0, dynamics });
        assert_eq!(applied, Applied::None);
        assert!(state.take_dynamics_dirty().is_none());
        assert_eq!(state.dynamics(), &DynamicPathState::default());
    }

    /// Геттеры читают последнее принятое значение (ADR-02, И-Р13) без
    /// привязки к разовому чтению `take_*_dirty`.
    #[test]
    fn getters_hold_last_accepted_values() {
        let mut state = UiAudioState::new();
        assert!(state.track().is_none());
        assert_eq!(state.format(), (44_100, 2));

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

    /// Формат до первого открытия — дефолт, повторяющий поведение моста С3.
    #[test]
    fn format_default_before_open() {
        let state = UiAudioState::new();
        assert_eq!(state.format(), (44_100, 2));
        assert!(state.path().is_none());
        assert!(state.is_stopped());
        assert!(!state.is_playing());
    }

    /// §3.1, ADR-05: `format()` читает запрошенный формат вывода из
    /// `path().output.requested` принятого пути — до первого открытия
    /// остаётся дефолт `(44_100, 2)`. Фикстура `test_signal_path` сама
    /// совпадает с дефолтом (44 100/2), поэтому запрошенный формат здесь
    /// заменяется на заведомо отличный (96 000/1), чтобы тест отличал
    /// значение, прочитанное из пути, от дефолта.
    #[test]
    fn format_follows_signal_path() {
        use std::num::NonZeroU8;

        use music_player_rs::audio::format::SampleRate;

        let mut state = UiAudioState::new();
        assert_eq!(state.format(), (44_100, 2));

        let mut path = (*test_signal_path(1)).clone();
        path.output.requested.rate = SampleRate::new(96_000).expect("96000 — валидная частота");
        path.output.requested.channels = NonZeroU8::new(1).expect("1 канал — валидно");

        state.begin_open(1);
        state.apply(opened_with_badge(1, Some(Arc::new(path)), BadgeState::NoStream));

        assert_eq!(state.format(), (96_000, 1));
    }

    /// С5-мост, §2.3, ADR-05: без построенного пути (Оптимальный/Строгий до
    /// С6) `format()` падает на принятый `stream_format` старого пути;
    /// следующее принятое `OpenFailed` сбрасывает его обратно на дефолт
    /// (ТЗ-56, та же семантика, что и у `path`).
    #[test]
    fn format_falls_back_to_stream_format_without_path() {
        use std::num::NonZeroU8;

        use music_player_rs::audio::format::{OutputSampleFormat, SampleRate, StreamFormat};

        let mut state = UiAudioState::new();
        assert_eq!(state.format(), (44_100, 2));

        let stream_format = StreamFormat {
            rate: SampleRate::new(96_000).expect("96000 — валидная частота"),
            sample: OutputSampleFormat::F32,
            channels: NonZeroU8::new(1).expect("1 канал — валидно"),
        };
        state.begin_open(1);
        state.apply(EngineEvent::Opened {
            req_gen: 1,
            info: track_info(),
            stream_format: Some(stream_format),
            path: None,
            badge: BadgeState::NoStream,
        });
        assert_eq!(state.format(), (96_000, 1));

        state.begin_open(2);
        state.apply(EngineEvent::OpenFailed { req_gen: 2, err: OpenError::DeviceLost });
        assert_eq!(state.format(), (44_100, 2));
    }

    /// Снимок `Blocked` для теста `skipped` (§2.4, ТЗ-80) — только то, что
    /// нужно, чтобы `plan()` не вызывать: минимальный `PathPlan` со
    /// Строгим режимом, остановленный на этапе «Выход».
    fn test_blocked() -> Blocked {
        use std::num::NonZeroU8;

        use music_player_rs::audio::error::Incompatibility;
        use music_player_rs::audio::format::{
            BitDepth, ChannelLayout, Codec, Container, OutputSampleFormat, SampleRate, SourceFormat, SourceKind,
            StreamFormat,
        };
        use music_player_rs::audio::path::{PathPlan, Route, StageId};
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
        let partial = PathPlan {
            mode: ModeKind::Strict,
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
        Blocked { at: StageId::Output, reason: Incompatibility::FloatUnsupported, partial }
    }

    /// ТЗ-56, ADR-02: `ended` взводится принятым `Ended` для активной сессии
    /// и снимается `clear_end` — повторяет тест моста С3
    /// `ended_flag_set_and_cleared`.
    #[test]
    fn ended_flag_set_and_cleared() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.apply(opened(1));
        assert!(!state.ended());

        let applied = state.apply(EngineEvent::Ended { session: 1 });
        assert_eq!(applied, Applied::Ended);
        assert!(state.ended());

        state.clear_end();
        assert!(!state.ended());
    }

    /// И-Р14, ADR-02: `take_opened` отдаёт принятый `Opened` один раз —
    /// повторяет тест моста С3 `take_opened_returns_once_after_opened_event`.
    #[test]
    fn take_opened_returns_once_after_opened_event() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        assert!(state.take_opened().is_none());

        state.apply(opened(1));
        assert!(state.take_opened().is_some());
        assert!(state.take_opened().is_none());
    }

    /// ТЗ-56, ADR-02: уведомление о резервировании ставится в очередь и
    /// снимается один раз — повторяет тест моста С3
    /// `reservation_notice_queued_and_polled_once`.
    #[test]
    fn reservation_notice_queued_and_polled_once() {
        let mut state = UiAudioState::new();
        assert!(state.poll_reservation().is_none());

        state.apply(EngineEvent::Notice(Notice::Reservation(ReservationEvent::Opened)));
        assert_eq!(state.poll_reservation(), Some(ReservationEvent::Opened));
        assert!(state.poll_reservation().is_none());
    }

    /// И-Р14, ADR-02: устаревшее `Opened` не взводит `take_opened` —
    /// повторяет тест моста С3 `stale_opened_event_is_ignored`.
    #[test]
    fn stale_opened_event_is_ignored() {
        let mut state = UiAudioState::new();
        state.begin_open(1);
        state.begin_open(2);

        state.apply(opened(1));
        assert!(state.take_opened().is_none());
    }

    /// ТЗ-79, ТЗ-80, §2.4: `skipped` взводится `Skipped(Incompatible)` и
    /// снимается следующим принятым `Opened` с построенным путём.
    #[test]
    fn skipped_set_by_incompatible_and_cleared_by_opened() {
        let mut state = UiAudioState::new();
        state.begin_open(1);

        state.apply(EngineEvent::Skipped {
            req_gen: 1,
            path: Arc::from(Path::new("/tmp/a.flac")),
            reason: SkipReason::Incompatible(test_blocked()),
        });
        assert!(state.skipped().is_some());

        state.begin_open(2);
        state.apply(opened_with_badge(2, Some(test_signal_path(1)), BadgeState::NoStream));
        assert!(state.skipped().is_none());
    }

    /// ТЗ-119, §6.19, AM1.0-B9 (шаг 72): `EngineEvent::Lock(Acquiring)`
    /// взводит `lock_acquiring` для строки состояния «Захват устройства…».
    #[test]
    fn lock_acquiring_set_by_acquiring_state() {
        use music_player_rs::audio::clock::ClockInstant;

        let mut state = UiAudioState::new();
        assert!(!state.lock_acquiring());

        let applied = state.apply(EngineEvent::Lock(LockState::Acquiring { started: ClockInstant::START }));
        assert_eq!(applied, Applied::None);
        assert!(state.lock_acquiring());
    }

    /// ТЗ-119, §6.19, AM1.0-B9 (шаг 72): `Held`/`Failed` снимают
    /// `lock_acquiring` — строка состояния не остаётся «застрявшей» на
    /// «Захват устройства…» после выхода из `Acquiring`.
    #[test]
    fn lock_acquiring_cleared_by_held_and_failed() {
        use music_player_rs::audio::backend::exclusive::ClaimInfo;
        use music_player_rs::audio::clock::ClockInstant;
        use music_player_rs::audio::error::CaptureFailure;

        let mut state = UiAudioState::new();
        state.apply(EngineEvent::Lock(LockState::Acquiring { started: ClockInstant::START }));
        assert!(state.lock_acquiring());

        let device: HwDeviceId = "hw:CARD=USB,DEV=0".parse().expect("валидный HwDeviceId");
        state.apply(EngineEvent::Lock(LockState::Held { info: ClaimInfo { device } }));
        assert!(!state.lock_acquiring());

        state.apply(EngineEvent::Lock(LockState::Acquiring { started: ClockInstant::START }));
        assert!(state.lock_acquiring());

        state.apply(EngineEvent::Lock(LockState::Failed(CaptureFailure::DeviceDisconnected)));
        assert!(!state.lock_acquiring());
    }

    /// ТЗ-125, §6.19 (шаг 74): до первого принятого `EngineEvent::Lock`
    /// `lock()` — `NotApplicable` (Совместимый режим, ТЗ-50).
    #[test]
    fn lock_is_not_applicable_before_first_event() {
        let state = UiAudioState::new();
        assert_eq!(state.lock(), &LockState::NotApplicable);
    }

    /// ТЗ-125, §6.19 (шаг 74): `lock()` хранит принятое состояние целиком,
    /// не только флаг «идёт захват».
    #[test]
    fn lock_holds_accepted_state() {
        let mut state = UiAudioState::new();

        state.apply(EngineEvent::Lock(LockState::Open));
        assert_eq!(state.lock(), &LockState::Open);
    }
}
