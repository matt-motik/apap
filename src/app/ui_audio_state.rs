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
//! `stream_desc()`) удалён из состояния UI. Формат вывода для визуализации
//! отсюда больше не читается: анализатор берёт его из заголовка границы
//! трека в tap (AM1.0-B12), поэтому `stream_format` из `Opened` не хранится.
//!
//! Шаг 72 (ТЗ-119, баг AM1.0-B9): `lock_acquiring` — единственное, что от
//! `EngineEvent::Lock` попадало в это состояние; полный вид замка остаётся
//! вне `UiAudioState` и строится отдельно в `apply_lock_view`.
//!
//! Шаг 74 (ТЗ-125, §6.19): UI-потоку нужен не только флаг «идёт захват»,
//! но и сам принятый `LockState` — чтобы решить, что делает Play при
//! открытом замке (шаг 75). Поле `lock` хранит последнее принятое
//! состояние целиком; `lock_acquiring()` сведён к `matches!` по нему.
//!
//! Баг AM1.0-B38, шаг 7: `hw_caps` хранит `CapsEntry` вместо голого
//! `Arc<DeviceCaps>` — различает «возможности известны» и «устройство
//! занято другим приложением» (итог фонового опроса `probe_caps`,
//! `EngineEvent::DeviceCapsProbed`). Запись, пришедшая из открытия устройства
//! (`DeviceCapsCached`), не вытесняется опросом ни в одну сторону — у движка
//! нет общего номера поколения между опросом и открытием.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use music_player_rs::audio::backend::catalog::DeviceCatalog;
use music_player_rs::audio::backend::exclusive::ProbeError;
use music_player_rs::audio::backend::{DeviceCaps, HwDeviceId};
use music_player_rs::audio::decoder::TrackInfo;
use music_player_rs::audio::error::{OpenError, Reaction, StreamFault};
use music_player_rs::audio::path::signal::{DynamicPathState, SignalPath};
use music_player_rs::audio::path::Blocked;
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

/// Запись кэша `hw_caps` (AM1.0-B38, шаг 7): различает «возможности
/// известны» и «устройство занято другим приложением», вместо голого
/// `Arc<DeviceCaps>`.
///
/// `Known.opened` — истина, если запись пришла из `DeviceCapsCached`
/// (устройство действительно было открыто этим приложением), и ложь, если
/// запись пришла из фонового опроса (`DeviceCapsProbed`, AM1.0-B38). Открытое
/// устройство — источник истины: более поздний по времени получения, но
/// возникший из фонового опроса результат (запущенного до открытия и
/// завершившегося после) не вытесняет такую запись — ни успехом, ни
/// «занято», ни ошибкой драйвера. У движка нет общего номера поколения между
/// `probe_caps` и открытием устройства, поэтому это единственный сигнал
/// порядка, доступный в UI-потоке.
#[derive(Clone, Debug, PartialEq)]
enum CapsEntry {
    Known { caps: Arc<DeviceCaps>, opened: bool },
    /// Устройство занято другим приложением (`ProbeError::Busy`) — снимается
    /// следующей принятой записью `Known` для того же устройства (открытием
    /// или успешным опросом).
    Busy,
}

impl CapsEntry {
    fn caps(&self) -> Option<&DeviceCaps> {
        match self {
            Self::Known { caps, .. } => Some(caps),
            Self::Busy => None,
        }
    }

    fn is_opened(&self) -> bool {
        matches!(self, Self::Known { opened: true, .. })
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
    /// `fault` — код `StreamFault`, пронесённый из `EngineEvent::DeviceLost`
    /// (AM1.0-B34, §6.20): `Some(StreamFault::Stalled)` различает «поток не
    /// запустился» от общей потери устройства в `app/mod.rs`.
    DeviceLost { reaction: Reaction, fault: Option<StreamFault> },
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
    badge: Written<BadgeState>,
    dynamics: DynamicPathState,
    /// Копия `dynamics`, «записанная в UI» (ТЗ-68, §3.5, шаг 78) — тот же
    /// смысл, что и `written` в `Written<T>`, но без обвязки generic:
    /// `dynamics()` должен возвращать `&DynamicPathState`, а не `Option`.
    // Читается только в `take_dynamics_dirty`.
    dynamics_written: Option<DynamicPathState>,
    /// Истина после принятого `Ended`, пока не снята `clear_end` (ТЗ-56,
    /// ADR-02).
    ended: bool,
    /// Вновь принятый `Opened`, ещё не забранный вызывающей стороной —
    /// снимается один раз через `take_opened` (И-Р14, ADR-02).
    opened: Option<Arc<TrackInfo>>,
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
    /// Кэш возможностей открытых аппаратных устройств по `HwDeviceId`
    /// (AM1.0-B30, решение 2026-10-24): наполняется из
    /// `EngineEvent::DeviceCapsCached`, пережитого открытием устройства, а
    /// не перечислением каталога — блок «Аудио» диалога настроек читает
    /// его для устройства, выбранного в редактируемом режиме, без
    /// повторного открытия. Запись для `id` сохраняется между открытиями
    /// разных треков того же устройства и между перечислениями каталога,
    /// пока устройство остаётся в `DeviceCatalog.hw` (обрезается в
    /// `EngineEvent::Devices`, когда устройство пропадает из списка).
    ///
    /// Шаг 7 (AM1.0-B38): значение — `CapsEntry`, различающая «возможности
    /// известны» (из открытия или фонового опроса) и «занято другим
    /// приложением» (из опроса) вместо голого `Arc<DeviceCaps>`.
    hw_caps: BTreeMap<HwDeviceId, CapsEntry>,
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
            badge: Written::default(),
            dynamics: DynamicPathState::default(),
            dynamics_written: None,
            ended: false,
            opened: None,
            skipped: None,
            lock: LockState::NotApplicable,
            hw_caps: BTreeMap::new(),
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
            EngineEvent::Opened { req_gen, info, stream_format: _, path, badge } => {
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
            EngineEvent::DeviceLost { reaction, fault } => Applied::DeviceLost { reaction, fault },
            EngineEvent::Devices(catalog) => {
                // Кэш возможностей переживает перечисление для устройств,
                // остающихся в списке (AM1.0-B30); запись устройства,
                // пропавшего из `hw` (отключено/не видно ALSA), удаляется —
                // иначе блок «Аудио» показывал бы возможности устройства,
                // которого больше нет.
                self.hw_caps.retain(|id, _| catalog.hw.iter().any(|dev| &dev.id == id));
                self.devices.set(DeviceSnapshot(catalog));
                Applied::None
            }
            // Возможности сняты при открытии устройства, не при
            // перечислении каталога (AM1.0-B30, решение 2026-10-24) —
            // кэшируются сразу, отдельно от `Devices`.
            EngineEvent::DeviceCapsCached { device, caps } => {
                // Открытие — источник истины (ТЗ по AM1.0-B38, см. `CapsEntry`):
                // запись всегда помечается `opened: true`, безусловно
                // вытесняя всё, что в кэше было до неё (включая «занято» от
                // опроса другого устройства, которое перед этим занимало
                // слот — хотя открытое устройство опросу недоступно).
                self.hw_caps.insert(device, CapsEntry::Known { caps, opened: true });
                Applied::None
            }
            // Фоновый опрос (AM1.0-B38, §2.3, шаг 7): открытая запись не
            // вытесняется ни одним из исходов — она либо свежее (опрос
            // открытого устройства не запускается), либо устарела (опрос
            // начался раньше открытия и завершился позже). `Driver(_)` не
            // меняет кэш вовсе — устройство остаётся «неизвестно», как и до
            // опроса.
            EngineEvent::DeviceCapsProbed { device, result } => {
                if self.hw_caps.get(&device).is_some_and(CapsEntry::is_opened) {
                    return Applied::None;
                }
                match result {
                    Ok(caps) => {
                        self.hw_caps.insert(device, CapsEntry::Known { caps, opened: false });
                    }
                    Err(ProbeError::Busy) => {
                        self.hw_caps.insert(device, CapsEntry::Busy);
                    }
                    Err(ProbeError::Driver(_)) => {}
                }
                Applied::None
            }
            // Автовыбор сохраняется приложением в настройках (ТЗ-143,
            // ТЗ-134, ADR-24, шаг 38) — одним снимком `settings.toml`, без
            // `EngineCmd::SetModeSettings` (у движка уже есть это значение,
            // повторная отправка вызвала бы петлю повторного открытия).
            EngineEvent::DeviceAutoSelected { mode, device } => Applied::DeviceAutoSelected { mode, device },
            EngineEvent::Notice(notice) => Applied::Notice(notice),
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

    /// Последний принятый каталог устройств движка (ADR-24, ТЗ-126), без
    /// снятия «грязного» флага — вкладка Audio диалога настроек строит из
    /// него список устройств Оптимального/Строгого режима (`DeviceCatalog.hw`),
    /// а не из перечисления cpal.
    pub fn devices(&self) -> Option<&Arc<DeviceCatalog>> {
        self.devices.get().map(|snap| &snap.0)
    }

    /// Кэшированные возможности аппаратного устройства `id`, если оно уже
    /// открывалось в этой сессии движка (AM1.0-B30, решение 2026-10-24) —
    /// блок «Аудио» диалога настроек строит из них «Частоты»/«Разрядность»/
    /// «DSD» устройства, выбранного в редактируемом режиме, без повторного
    /// открытия. `None` — устройство этого режима ещё не открывалось.
    // Вызывающая сторона (`ui_manager.rs`) подключается следующим шагом
    // (AM1.0-B30 step 2) — метод уже покрыт тестами кэша ниже.
    pub fn hw_caps(&self, id: &HwDeviceId) -> Option<&DeviceCaps> {
        self.hw_caps.get(id).and_then(CapsEntry::caps)
    }

    /// Истина, если устройство `id` помечено занятым другим приложением
    /// фоновым опросом (AM1.0-B38, шаг 7, `ProbeError::Busy`) — блок «Аудио»
    /// (шаг 8) показывает для него «неизвестно — устройство занято» вместо
    /// обычного «неизвестно». Снимается следующей принятой записью
    /// `CapsEntry::Known` для того же устройства (открытием или успешным
    /// повторным опросом).
    //
    // Подключена в `ui_manager.rs::device_busy_for_mode` →
    // `sync_device_info_to_ui` (AM1.0-B38, шаг 11).
    pub fn device_busy(&self, id: &HwDeviceId) -> bool {
        matches!(self.hw_caps.get(id), Some(CapsEntry::Busy))
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
    /// чтения (ТЗ-68, §3.5, И-Р13, шаг 78/84) — тот же смысл, что и
    /// `Written::take_dirty`, но без Option-обвязки текущего значения.
    // Потребитель — `apply_engine_event` (`mod.rs`, шаг 84): инвалидирует
    // кэш отчёта «Путь сигнала» при изменении динамики.
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
        use music_player_rs::audio::path::{DacCaps, OpenedFacts, OsStage, PathPlan, ReservationView, Route};
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
            dac: DacCaps::default(),
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

    /// AM1.0-B34 (шаг 4, §6.20): `fault` проходит через `apply` без фильтра
    /// по `req_gen` — он относится к потоку вывода, а не к сессии открытия.
    #[test]
    fn device_lost_carries_fault_through() {
        let mut state = UiAudioState::new();

        let applied = state.apply(EngineEvent::DeviceLost {
            reaction: Reaction::StopWithError,
            fault: Some(StreamFault::Stalled),
        });

        assert_eq!(
            applied,
            Applied::DeviceLost { reaction: Reaction::StopWithError, fault: Some(StreamFault::Stalled) }
        );
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

    /// До первого открытия — нет пути, транспорт остановлен.
    #[test]
    fn state_before_open() {
        let state = UiAudioState::new();
        assert!(state.path().is_none());
        assert!(state.is_stopped());
        assert!(!state.is_playing());
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
        use music_player_rs::audio::path::{DacCaps, PathPlan, Route, StageId};
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
            dac: DacCaps::default(),
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

    fn fake_hw_caps() -> DeviceCaps {
        use music_player_rs::audio::backend::{AccessKind, OutputApi, RateSet};
        use music_player_rs::audio::format::{OutputSampleFormat, SampleRate};
        use smallvec::SmallVec;

        DeviceCaps {
            api: OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            name: "RU3".to_owned(),
            rates: RateSet::Discrete(SmallVec::from_slice(&[SampleRate::new(44_100).expect("44100 валидна")])),
            formats: SmallVec::from_slice(&[OutputSampleFormat::S24_3Le]),
            channels: SmallVec::from_slice(&[2]),
        }
    }

    fn fake_hw_catalog(ids: &[&HwDeviceId]) -> Arc<DeviceCatalog> {
        use music_player_rs::audio::backend::catalog::HwDeviceInfo;

        Arc::new(DeviceCatalog {
            hw: ids
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let card = u32::try_from(i).expect("card id fits u32 in test");
                    HwDeviceInfo { id: (*id).clone(), card, device: 0, name: "RU3".to_owned() }
                })
                .collect(),
            ..Default::default()
        })
    }

    /// AM1.0-B30: `DeviceCapsCached` кэширует возможности по `HwDeviceId` —
    /// без них отдельного события (например, `Opened`) блок «Аудио» не
    /// знает частот/форматов/DSD устройства.
    #[test]
    fn hw_caps_cached_by_device_caps_cached_event() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");
        assert!(state.hw_caps(&device).is_none());

        state.apply(EngineEvent::DeviceCapsCached { device: device.clone(), caps: Arc::new(fake_hw_caps()) });
        assert_eq!(state.hw_caps(&device), Some(&fake_hw_caps()));
    }

    /// AM1.0-B30: кэш переживает перечисление каталога, пока устройство
    /// остаётся в `DeviceCatalog.hw` — переоткрывать устройство для показа
    /// возможностей блок «Аудио» не должен.
    #[test]
    fn hw_caps_survive_catalog_refresh_when_device_still_present() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");
        state.apply(EngineEvent::DeviceCapsCached { device: device.clone(), caps: Arc::new(fake_hw_caps()) });

        state.apply(EngineEvent::Devices(fake_hw_catalog(&[&device])));
        assert_eq!(state.hw_caps(&device), Some(&fake_hw_caps()));
    }

    /// AM1.0-B30: устройство пропало из перечисления (отключено/не видно
    /// ALSA) — его кэш удаляется, иначе блок «Аудио» показал бы
    /// возможности устройства, которого больше нет.
    #[test]
    fn hw_caps_evicted_when_device_leaves_catalog() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");
        let other: HwDeviceId = "hw:CARD=M2,DEV=0".parse().expect("валидный HwDeviceId");
        state.apply(EngineEvent::DeviceCapsCached { device: device.clone(), caps: Arc::new(fake_hw_caps()) });

        state.apply(EngineEvent::Devices(fake_hw_catalog(&[&other])));
        assert!(state.hw_caps(&device).is_none());
    }

    fn other_caps() -> DeviceCaps {
        use music_player_rs::audio::backend::{AccessKind, OutputApi, RateSet};
        use music_player_rs::audio::format::{OutputSampleFormat, SampleRate};
        use smallvec::SmallVec;

        DeviceCaps {
            api: OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            name: "other".to_owned(),
            rates: RateSet::Discrete(SmallVec::from_slice(&[SampleRate::new(48_000).expect("48000 валидна")])),
            formats: SmallVec::from_slice(&[OutputSampleFormat::S32Le]),
            channels: SmallVec::from_slice(&[2]),
        }
    }

    /// AM1.0-B38, шаг 7: успешный фоновый опрос кэширует возможности тем же
    /// `hw_caps`, что и открытие устройства (DoD задачи).
    #[test]
    fn probed_ok_caches_caps() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");
        assert!(state.hw_caps(&device).is_none());

        state.apply(EngineEvent::DeviceCapsProbed { device: device.clone(), result: Ok(Arc::new(fake_hw_caps())) });
        assert_eq!(state.hw_caps(&device), Some(&fake_hw_caps()));
        assert!(!state.device_busy(&device));
    }

    /// AM1.0-B38, шаг 7: `ProbeError::Busy` помечает устройство занятым,
    /// `device_busy` — истина, `hw_caps` остаётся `None`.
    #[test]
    fn probed_busy_marks_device_busy() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");

        state.apply(EngineEvent::DeviceCapsProbed { device: device.clone(), result: Err(ProbeError::Busy) });
        assert!(state.device_busy(&device));
        assert!(state.hw_caps(&device).is_none());
    }

    /// AM1.0-B38, шаг 7: занятая запись снимается следующим успешным
    /// исходом (повторный опрос или открытие устройства).
    #[test]
    fn busy_cleared_when_caps_arrive() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");
        state.apply(EngineEvent::DeviceCapsProbed { device: device.clone(), result: Err(ProbeError::Busy) });
        assert!(state.device_busy(&device));

        state.apply(EngineEvent::DeviceCapsProbed { device: device.clone(), result: Ok(Arc::new(fake_hw_caps())) });
        assert!(!state.device_busy(&device));
        assert_eq!(state.hw_caps(&device), Some(&fake_hw_caps()));
    }

    /// AM1.0-B38, шаг 7: `ProbeError::Driver(_)` не меняет кэш — устройство
    /// остаётся «неизвестно», не «занято».
    #[test]
    fn probed_driver_error_leaves_unknown() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");

        state.apply(EngineEvent::DeviceCapsProbed {
            device: device.clone(),
            result: Err(ProbeError::Driver("boom".to_owned())),
        });
        assert!(state.hw_caps(&device).is_none());
        assert!(!state.device_busy(&device));
    }

    /// AM1.0-B38, шаг 7, DoD: открытая запись (`DeviceCapsCached`) — источник
    /// истины; более поздний по приходу, но устаревший по сути результат
    /// фонового опроса (успех, «занято» или ошибка драйвера) её не вытесняет.
    #[test]
    fn opened_caps_not_overwritten_by_stale_probe() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");
        state.apply(EngineEvent::DeviceCapsCached { device: device.clone(), caps: Arc::new(fake_hw_caps()) });

        state.apply(EngineEvent::DeviceCapsProbed { device: device.clone(), result: Ok(Arc::new(other_caps())) });
        assert_eq!(state.hw_caps(&device), Some(&fake_hw_caps()));

        state.apply(EngineEvent::DeviceCapsProbed { device: device.clone(), result: Err(ProbeError::Busy) });
        assert_eq!(state.hw_caps(&device), Some(&fake_hw_caps()));
        assert!(!state.device_busy(&device));
    }

    /// AM1.0-B38, шаг 7: открытие вытесняет «занято», принятое от опроса
    /// другого устройства, занявшего слот до открытия.
    #[test]
    fn opened_overwrites_busy_entry() {
        let mut state = UiAudioState::new();
        let device: HwDeviceId = "hw:CARD=RU3,DEV=0".parse().expect("валидный HwDeviceId");
        state.apply(EngineEvent::DeviceCapsProbed { device: device.clone(), result: Err(ProbeError::Busy) });
        assert!(state.device_busy(&device));

        state.apply(EngineEvent::DeviceCapsCached { device: device.clone(), caps: Arc::new(fake_hw_caps()) });
        assert!(!state.device_busy(&device));
        assert_eq!(state.hw_caps(&device), Some(&fake_hw_caps()));
    }
}
