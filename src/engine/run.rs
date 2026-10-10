//! Цикл потока `apap-engine` (§6.1, ADR-01, ADR-02, ADR-20, ТЗ-103). Мост С3:
//! `Engine` строится внутри самого потока (`Player` не обязан быть `Send`,
//! §2.10) и держит легаси `Player` целиком вместо `SignalPath`/сессии (С4/С5).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::audio::backend::catalog::DeviceCatalog;
use crate::audio::clock::ClockInstant;
use crate::audio::error::{CaptureFailure, EngineFault, FileError, OpenError};
use crate::audio::player::{Player, ReservationEvent};
use crate::engine::deps::EngineDeps;
use crate::engine::legacy_path::legacy_audio;
use crate::engine::messages::{EngineCmd, EngineEvent, LegacyAudio, Notice, SkipReason, TransportState};
use crate::settings::params::{ApplyKind, ModeSettingsDiff};
use crate::settings::playback::{ModeKind, ModeSettings};

/// Минимальный интервал между событиями `Position` (§6.1, И-Р13).
const POSITION_INTERVAL: Duration = Duration::from_millis(100);

/// Решение `EngineCmd::SetModeSettings` по диффу активного режима и
/// состоянию транспорта (§6.18, И-Р26). `Store` — без переоткрытия: пустой
/// дифф и изменения с `ApplyKind::Memory` не трогают `Player`, а на
/// остановленном транспорте нет сессии для переоткрытия, поэтому параметры
/// только копируются в `Player` для следующего `Open`. `ReopenAtPosition` и
/// `SwitchDevice` — одно переоткрытие текущего трека старым путём;
/// `SwitchDevice` поглощает остальные изменения активного режима, т.к. он
/// сильнее по порядку `ApplyKind` (И-Р26).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModeApplyAction {
    Store,
    ReopenAtPosition,
    SwitchDevice,
}

/// Чистая часть решения `SetModeSettings` (§6.18, И-Р26), без доступа к
/// `Player` — тестируется без реального аудио-устройства (ТЗ-114).
/// Изменения неактивного режима (`diff.strongest(active)` не их учитывает)
/// не дают эффекта.
pub(crate) fn mode_apply_action(diff: Option<&ModeSettingsDiff>, active: ModeKind, stopped: bool) -> ModeApplyAction {
    match diff.and_then(|d| d.strongest(active)) {
        None | Some(ApplyKind::Memory) => ModeApplyAction::Store,
        Some(ApplyKind::ReopenAtPosition) | Some(ApplyKind::SwitchDevice) if stopped => ModeApplyAction::Store,
        Some(ApplyKind::ReopenAtPosition) => ModeApplyAction::ReopenAtPosition,
        Some(ApplyKind::SwitchDevice) => ModeApplyAction::SwitchDevice,
    }
}

/// Действующая громкость по правилу источников громкости (§6.18, ОВС-18…
/// ОВС-20, ADR-23, ТЗ-41, ТЗ-140), без доступа к `Player` — тестируется без
/// реального аудио-устройства (ТЗ-114). Mute гасит сигнал в любом режиме
/// (ОВС-18). Иначе в Строгом режиме и в Оптимальном с «Фиксировать громкость
/// на 100 %» (`volume_lock`) ступени громкости нет — эффективная громкость
/// `1.0` независимо от запрошенной (ADR-23, ТЗ-41, ТЗ-140); в Совместимом и
/// в Оптимальном без фиксации громкость — запрошенное значение.
pub(crate) fn effective_gain(settings: &ModeSettings, active: ModeKind, volume_gain: f32, muted: bool) -> f32 {
    if muted {
        return 0.0;
    }
    match active {
        ModeKind::Strict => 1.0,
        ModeKind::Optimal if settings.optimal.volume_lock => 1.0,
        _ => volume_gain,
    }
}

/// Цикл обработки команд (мост С3): владеет `Player` целиком, пока
/// `SignalPath`/`BadgeState`/`DeviceCatalog.hw` не появились (С4…С6).
struct Engine {
    player: Player,
    deps: EngineDeps,
    /// `req_gen` и путь последнего успешно открытого трека — для `SetDevice`
    /// (переоткрытие на новом устройстве) и для `req_gen` в `OpenFailed`.
    current: Option<(u64, Arc<Path>)>,
    /// Кэш перечисления Shared-устройств (ADR-16, §2.3); пополняется при
    /// старте движка, по сигналу `DeviceWatcher` и по команде `RefreshDevices`
    /// (ТЗ-104, ТЗ-105, §6.2 п.3).
    catalog: DeviceCatalog,
    /// Флаг «список устройств мог измениться», выставляемый колбэком
    /// `DeviceWatcher` из произвольного потока (ADR-16, ТЗ-105); `poll_session`
    /// снимает его и перечисляет устройства не чаще одного раза на событие.
    devices_dirty: Arc<AtomicBool>,
    /// Явная остановка транспорта (ADR-20, мост С3): легаси `Player` держит
    /// декодер загруженным и после `stop()` (`has_decoder()` остаётся
    /// `true`), поэтому `Stopped` отслеживается здесь, а не выводится из
    /// `Player`. `true` до первого успешного `Open`, после `Stop` и после
    /// отказа/потери резервирования.
    stopped: bool,
    /// Последнее отправленное состояние `Transport` — для дедупликации
    /// (§6.1 п. 3, И-Р13): событие не шлётся повторно с тем же состоянием.
    last_transport: Option<TransportState>,
    /// Последняя отправленная позиция — для дедупликации `Position`.
    last_pos: Option<f64>,
    /// Момент последней отправки `Position` — минимальный интервал 100 мс
    /// (§6.1 п. 4, И-Р13).
    last_pos_at: Option<ClockInstant>,
    /// Мид-трековый отказ декодера уже разобран в текущей сессии (ТЗ-87,
    /// §6.18 шаг 19): `poll_session` уже отправил `Skipped` и не должен затем
    /// отправить `Ended` по тому же `ended()` (мид-трековый отказ — это
    /// пропуск, а не штатный конец трека). Сбрасывается на следующий успешный
    /// `Open`.
    session_failed: bool,
    /// Копия настроек всех режимов (ОВС-14, §6.18): обновляется по
    /// `SetModeSettings` целиком, независимо от того, требует ли изменение
    /// переоткрытия.
    mode_settings: ModeSettings,
    /// Активный режим (ADR-04, §2.2); меняется только `SetActiveMode`.
    active_mode: ModeKind,
    /// Последняя запрошенная через `SetVolume` громкость (§6.18, ОВС-20):
    /// хранится независимо от того, применилась ли она к `Player` —
    /// restore при снятии фиксации/смене режима в `apply_gain`.
    last_volume: f32,
    /// Последнее запрошенное через `SetMuted` состояние mute (ОВС-18):
    /// mute гасит сигнал во всех режимах, `player.set_muted` вызывается
    /// напрямую, вне правила `effective_gain`.
    last_muted: bool,
    /// Возможности бэкенда (§2.3, ТЗ-109): определяет доступность
    /// `active_mode` для проверки в `Open` (§6.18 п. 2). Снимок на момент
    /// запуска потока — платформа не меняется на ходу.
    caps: BackendCaps,
}

impl Engine {
    fn new(deps: EngineDeps, caps: BackendCaps) -> Engine {
        let mut player = Player::new();
        player.set_spawner(Arc::clone(&deps.spawner));
        let player_volume = player.volume();
        Engine {
            player,
            deps,
            current: None,
            catalog: DeviceCatalog::default(),
            devices_dirty: Arc::new(AtomicBool::new(false)),
            stopped: true,
            last_transport: None,
            last_pos: None,
            last_pos_at: None,
            session_failed: false,
            mode_settings: ModeSettings::default(),
            active_mode: ModeKind::default(),
            last_volume: player_volume,
            last_muted: false,
            caps,
        }
    }

    /// Цикл движка (§6.1): команда — сразу `handle`, тайм-аут 20 мс —
    /// следующая итерация; после каждой обработанной команды и на каждом
    /// тайм-ауте — опрос сессии `poll_session` (`Position`/`Transport`/
    /// `Ended`/резервирование, §6.1 п. 1–5, И-Р13, ТЗ-60, ТЗ-102). Опрос не
    /// выполняется после `Shutdown`/`Disconnected` — цикл уже завершается.
    ///
    /// Перед циклом (§6.2 п.3, ТЗ-104, ТЗ-105): запускается `DeviceWatcher` и
    /// выполняется первое перечисление устройств — UI должен получить ответ
    /// при старте даже при пустом списке (заменяет прежний busy-wait пробник
    /// вывода на стороне UI).
    fn run(mut self, rx: Receiver<EngineCmd>) {
        let dirty = Arc::clone(&self.devices_dirty);
        self.deps.watcher.start(Box::new(move || {
            dirty.store(true, Ordering::Release);
        }));
        self.refresh_devices(true);

        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(cmd) => {
                    if self.handle(cmd) {
                        break;
                    }
                    self.poll_session();
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.poll_session();
                    continue;
                }
                // UI исчез без Shutdown (§6.1): реагируем так же, как на Shutdown.
                Err(RecvTimeoutError::Disconnected) => {
                    self.handle(EngineCmd::Shutdown);
                    break;
                }
            }
        }
    }

    /// Обрабатывает одну команду. Возвращает `true`, если цикл должен
    /// завершиться (`Shutdown`).
    fn handle(&mut self, cmd: EngineCmd) -> bool {
        match cmd {
            EngineCmd::Open { req_gen, path, start_secs, autoplay } => {
                // §6.18 п. 2 (ТЗ-109, ТЗ-111): режим недоступен на платформе —
                // без захвата и без открытия файла, сохранённый активный
                // режим не меняется (§6.2 п. 5).
                if !self.caps.available(self.active_mode) {
                    self.deps.events.emit(EngineEvent::OpenFailed {
                        req_gen,
                        err: OpenError::ModeUnavailable(self.active_mode),
                    });
                    return false;
                }
                // §6.18 шаг 3: сначала только заголовок через `SourceOpener`,
                // без касания `Player` (ТЗ-86, ТЗ-103, ADR-20). Ошибка —
                // классифицированный `FileError`, сразу `Skipped`.
                if let Err(e) = self.deps.sources.probe(&path) {
                    self.deps.events.emit(EngineEvent::Skipped {
                        req_gen,
                        path,
                        reason: SkipReason::File(e),
                    });
                    return false;
                }
                match self.player.open(&path) {
                    Ok(info) => {
                        if start_secs > 0.0 {
                            self.player.seek(start_secs);
                        }
                        if autoplay {
                            self.player.play();
                        }
                        self.current = Some((req_gen, Arc::clone(&path)));
                        self.stopped = false;
                        self.last_pos = None;
                        self.session_failed = false;
                        let stream = self.player.stream_desc().cloned();
                        self.deps.events.emit(EngineEvent::Opened { req_gen, info, stream });
                        self.refresh_transport();
                    }
                    Err(msg) => {
                        // §6.18 шаг 19 (ТЗ-87, ТЗ-88): отказ запуска потока
                        // `apap-decode` (`ThreadSpawner`) — внутренняя ошибка
                        // движка, а не файла; иначе — мост С3: легаси `Player`
                        // пока отдаёт только текст, заворачиваем в `Unsupported`.
                        match self.player.take_fault() {
                            Some(fault) => {
                                self.deps.events.emit(EngineEvent::OpenFailed {
                                    req_gen,
                                    err: OpenError::Internal(fault),
                                });
                            }
                            None => {
                                self.deps.events.emit(EngineEvent::Skipped {
                                    req_gen,
                                    path,
                                    reason: SkipReason::File(FileError::Unsupported { codec: msg }),
                                });
                            }
                        }
                    }
                }
            }
            EngineCmd::Play => {
                self.player.play();
                self.stopped = false;
                self.refresh_transport();
            }
            EngineCmd::Pause => {
                // `Player::toggle` переключает play/pause; команда `Pause`
                // не должна запускать воспроизведение, если оно уже на паузе
                // или остановлено, поэтому переключаем только из «играет».
                if self.player.is_playing() {
                    self.player.toggle();
                }
                self.refresh_transport();
            }
            EngineCmd::Stop => {
                self.player.stop();
                self.stopped = true;
                self.refresh_transport();
            }
            EngineCmd::Seek { secs } => {
                self.player.seek(secs);
            }
            EngineCmd::SetVolume(v) => {
                self.last_volume = v;
                self.apply_gain();
            }
            EngineCmd::SetMuted(m) => {
                // Mute гасит сигнал во всех режимах (ОВС-18) напрямую через
                // `Player`, минуя `effective_gain`/`apply_gain`.
                self.last_muted = m;
                self.player.set_muted(m);
            }
            EngineCmd::SetDevice { id } => {
                let (req_gen, path) = match &self.current {
                    Some((gen, path)) => (*gen, Some(path.as_ref())),
                    None => (0, None),
                };
                let (_, pos, _) = self.player.snapshot();
                match self.player.set_device(id, path, pos) {
                    Ok(()) => self.refresh_transport(),
                    Err(_msg) => {
                        // Мост С3: `set_device` возвращает текст ошибки, а не
                        // классифицированный `CaptureFailure` (§6.6 — будущий
                        // шаг); ближайший по смыслу вариант — потеря устройства.
                        self.deps
                            .events
                            .emit(EngineEvent::OpenFailed { req_gen, err: OpenError::DeviceLost });
                    }
                }
            }
            EngineCmd::RefreshDevices => {
                self.refresh_devices(false);
            }
            EngineCmd::ReleaseExclusive => {
                self.player.release_if_exclusive();
                self.refresh_transport();
            }
            EngineCmd::SetVizTap(on) => {
                self.player.set_viz_tap_active(on);
            }
            EngineCmd::AttachVizTap(producer) => {
                self.player.set_viz_tap(producer);
            }
            EngineCmd::SetLegacyAudio(audio) => {
                // Применение старых настроек звука в памяти движка без
                // файлового I/O (ТЗ-134, И-Р24): сеттеры `Player` только
                // пишут поля, применяемые на следующем `open`; `bit_perfect` и
                // `dither` при изменении переоткрывают поток с текущей позиции
                // (I/O устройства, не настроек). Сохранение — дело UI.
                // Заменяется `SetModeSettings`/`SetActiveMode` в С4.
                self.player.set_exclusive_mode(audio.exclusive_mode);
                self.player.set_fallback_policy(audio.fallback_policy);
                self.player.set_dsd_mode(audio.dsd_mode);
                self.player.set_resampler_mode(audio.resampler_mode);
                self.player.set_resampler_algorithm(audio.resampler_algorithm);
                self.player.set_fixed_rate(audio.fixed_rate);
                self.player.set_prefer_family(audio.prefer_family);
                self.player.set_fallback_rate(audio.fallback_rate);
                self.player.set_ring_buffer_ms(audio.ring_buffer_ms);
                self.player.set_bit_perfect(audio.bit_perfect);
                self.player.set_dither(audio.dither);
            }
            EngineCmd::SetModeSettings(update) => {
                self.mode_settings = update.settings;
                // Снятие/включение `volume_lock` активного режима меняет
                // эффективную громкость без отдельного `SetVolume` (§6.18,
                // ОВС-19).
                self.apply_gain();
                let strongest = update.diff.as_ref().and_then(|d| d.strongest(self.active_mode));
                match mode_apply_action(update.diff.as_ref(), self.active_mode, self.stopped) {
                    // `Store`, вызванный остановленным транспортом при
                    // изменении с реальным эффектом (§6.18): нет сессии для
                    // переоткрытия — чистая запись параметров старого пути,
                    // применится на следующем `Open` (как `apply_active_mode`
                    // при `self.stopped` сегодня).
                    ModeApplyAction::Store
                        if matches!(strongest, Some(ApplyKind::ReopenAtPosition) | Some(ApplyKind::SwitchDevice)) =>
                    {
                        let (legacy, device) = legacy_audio(&self.mode_settings, self.active_mode);
                        self.apply_legacy_fields(&legacy, device);
                    }
                    // Пустой дифф или изменение с `ApplyKind::Memory` —
                    // настройки уже скопированы выше, больше ничего не нужно.
                    ModeApplyAction::Store => {}
                    ModeApplyAction::ReopenAtPosition | ModeApplyAction::SwitchDevice => {
                        self.apply_active_mode();
                    }
                }
            }
            EngineCmd::SetActiveMode(mode) => {
                if mode != self.active_mode {
                    self.active_mode = mode;
                    self.store_active_mode();
                    self.apply_gain();
                }
            }
            EngineCmd::Shutdown => {
                self.shutdown();
                return true;
            }
        }
        false
    }

    /// Пересчитывает и применяет громкость к `Player` по
    /// `effective_gain(mode_settings, active_mode, last_volume, last_muted)`
    /// (§6.18, ОВС-18…ОВС-20, ADR-23): вызывается при `SetVolume`, смене
    /// активного режима и `SetModeSettings`, чтобы снятие фиксации громкости
    /// или выход из Строгого режима восстанавливал сохранённое значение
    /// `last_volume` без отдельного `SetVolume` от UI.
    fn apply_gain(&mut self) {
        let gain = effective_gain(&self.mode_settings, self.active_mode, self.last_volume, self.last_muted);
        self.player.set_volume(gain);
    }

    /// Применяет `self.mode_settings`/`self.active_mode` к старому пути для
    /// `EngineCmd::SetModeSettings` (ОВС-14, ОВС-17, §6.18, ADR-22): при
    /// `Stopped` — только копия полей `Player` (нет сессии, переоткрывать
    /// нечего — применится на следующем `Open`); иначе одно переоткрытие на
    /// текущей позиции с сохранением играет/на паузе через
    /// `reopen_with_legacy`. `EngineCmd::SetActiveMode` переоткрытие здесь не
    /// запускает — см. `store_active_mode`.
    fn apply_active_mode(&mut self) {
        let (legacy, device) = legacy_audio(&self.mode_settings, self.active_mode);
        if self.stopped {
            self.apply_legacy_fields(&legacy, device);
        } else {
            self.reopen_with_legacy(legacy, device);
        }
    }

    /// Смена активного режима без переоткрытия движком (ТЗ-20, ОВ-10, §6.18
    /// строка 2399, ТС-9): по спецификации UI сама шлёт следом `Open` того
    /// же трека с текущей позиции — переоткрытие остаётся за ней. Если
    /// переоткрыть здесь (как `apply_active_mode`), а затем снова в
    /// `Open` от UI, получится два переоткрытия подряд — слышимый щелчок.
    /// Поэтому только копия полей старого пути: `release_engine` снимает
    /// поток (если он был открыт; на уже остановленном/безпотоковом движке
    /// не делает ничего), после чего `set_bit_perfect`/`set_dither` в
    /// `apply_legacy_fields` — чистая запись без собственного
    /// переоткрытия. Применится на следующем `Open`.
    fn store_active_mode(&mut self) {
        let (legacy, device) = legacy_audio(&self.mode_settings, self.active_mode);
        self.player.release_engine();
        self.apply_legacy_fields(&legacy, device);
    }

    /// Пишет поля `Player`, соответствующие старому пути (ТЗ-134, И-Р24).
    /// Сеттеры `bit_perfect`/`dither` переоткрывают поток сами, если он
    /// сейчас открыт (`self.player`'s `reopen_current`) — вызывающая сторона
    /// обязана сначала `release_engine`, если нужно ровно одно переоткрытие
    /// (см. `reopen_with_legacy`).
    fn apply_legacy_fields(&mut self, legacy: &LegacyAudio, device: Option<String>) {
        if let Some(name) = device {
            self.player.set_preferred_device(name);
        }
        self.player.set_exclusive_mode(legacy.exclusive_mode);
        self.player.set_fallback_policy(legacy.fallback_policy);
        self.player.set_dsd_mode(legacy.dsd_mode);
        self.player.set_resampler_mode(legacy.resampler_mode);
        self.player.set_resampler_algorithm(legacy.resampler_algorithm);
        self.player.set_fixed_rate(legacy.fixed_rate);
        self.player.set_prefer_family(legacy.prefer_family);
        self.player.set_fallback_rate(legacy.fallback_rate);
        self.player.set_ring_buffer_ms(legacy.ring_buffer_ms);
        self.player.set_bit_perfect(legacy.bit_perfect);
        self.player.set_dither(legacy.dither);
    }

    /// Одно переоткрытие текущего трека с новыми параметрами старого пути
    /// (`ApplyKind::ReopenAtPosition`/`SwitchDevice`, §6.18): `release_engine`
    /// снимает поток заранее, поэтому сеттеры `bit_perfect`/`dither` в
    /// `apply_legacy_fields` становятся чистой записью (их собственное
    /// `reopen_current` не срабатывает при снятом потоке) — переоткрытие
    /// ниже остаётся единственным, даже если изменились оба поля сразу
    /// (например, при смене режима). Позиция и играет/на паузе — как в
    /// `EngineCmd::Open`.
    fn reopen_with_legacy(&mut self, legacy: LegacyAudio, device: Option<String>) {
        let Some((req_gen, path)) = self.current.clone() else {
            self.apply_legacy_fields(&legacy, device);
            return;
        };
        let was_playing = self.player.is_playing();
        let (_, pos, _) = self.player.snapshot();
        self.player.release_engine();
        self.apply_legacy_fields(&legacy, device);
        match self.player.open(&path) {
            Ok(_info) => {
                if pos > 0.0 {
                    self.player.seek(pos);
                }
                if was_playing {
                    self.player.play();
                }
                self.refresh_transport();
            }
            Err(_msg) => {
                // Мост С3: как `SetDevice` — текст ошибки без классификации
                // (§6.6 — будущий шаг), ближайший по смыслу вариант — потеря
                // устройства.
                self.stopped = true;
                self.deps
                    .events
                    .emit(EngineEvent::OpenFailed { req_gen, err: OpenError::DeviceLost });
                self.refresh_transport();
            }
        }
    }

    /// Выход (§6.28 п. 2 в рамках моста, ТЗ-45): остановка транспорта
    /// (колбэк пишет тишину) → `release_engine` — сначала поток вывода
    /// (закрыть PCM, затем снять резервирование, И-Р1), затем декодер →
    /// `ShutdownComplete`. `CancelTest` и `lock_step(Shutdown)` появятся с
    /// тестом и замком (С8/С9). `PersistStore::save` здесь не вызывается:
    /// данных `bp_tests.toml`/`track_marks.toml` в движке моста ещё нет, а
    /// запись пустого содержимого затёрла бы файлы; запись добавляется вместе
    /// с их владельцем. `settings.toml`/`state.toml` пишет UI (ОВС-16).
    fn shutdown(&mut self) {
        if self.player.is_playing() {
            self.player.toggle();
        }
        self.player.release_engine();
        self.current = None;
        self.stopped = true;
        self.deps.events.emit(EngineEvent::ShutdownComplete);
    }

    /// Перечисляет Shared-устройства и уведомляет UI (ТЗ-104, ТЗ-105, ADR-16,
    /// §6.2 п.3). `force_emit` шлёт `Devices` даже без изменения списка —
    /// нужно для первого перечисления при старте движка; по `RefreshDevices`
    /// и по сигналу `DeviceWatcher` событие шлётся только при изменении.
    fn refresh_devices(&mut self, force_emit: bool) {
        match self.catalog.refresh(self.deps.shared.as_mut()) {
            Ok(changed) => {
                if changed || force_emit {
                    self.deps.events.emit(EngineEvent::Devices(Arc::new(self.catalog.clone())));
                }
            }
            Err(e) => {
                self.deps.events.emit(EngineEvent::Notice(Notice::DevicesUnavailable(e)));
            }
        }
    }

    /// `Transport` по текущему состоянию (мост С3): `self.stopped` имеет
    /// приоритет (у легаси `Player` декодер остаётся загруженным и после
    /// `stop()`, `has_decoder()` не различает «остановлено» и «на паузе»);
    /// иначе `Playing`, если идёт воспроизведение, `Paused`, если трек
    /// загружен, но не играет, иначе `Stopped`. Шлёт событие только при
    /// изменении состояния (§6.1 п. 3, И-Р13) — повторный вызов с тем же
    /// состоянием не даёт дубликата.
    fn refresh_transport(&mut self) {
        let state = if self.stopped {
            TransportState::Stopped
        } else if self.player.is_playing() {
            TransportState::Playing
        } else if self.player.has_decoder() {
            TransportState::Paused
        } else {
            TransportState::Stopped
        };
        if self.last_transport != Some(state) {
            self.last_transport = Some(state);
            self.deps.events.emit(EngineEvent::Transport { state });
        }
    }

    /// Опрос сессии на каждой итерации цикла (§6.1 п. 1–5, И-Р13, ТЗ-60,
    /// ТЗ-102): только чтение состояния `Player`, без I/O (перечисление
    /// устройств по сигналу `DeviceWatcher` — исключение, ТЗ-105). Порядок —
    /// каталог устройств, резервирование, мид-трековый отказ декодера, затем
    /// конец трека, затем транспорт, затем позиция.
    fn poll_session(&mut self) {
        if self.devices_dirty.swap(false, Ordering::Acquire) {
            self.refresh_devices(false);
        }

        if let Some(ev) = self.player.poll_reservation() {
            self.deps.events.emit(EngineEvent::Notice(Notice::Reservation(ev.clone())));
            match ev {
                ReservationEvent::Opened => {
                    self.stopped = false;
                    self.refresh_transport();
                }
                ReservationEvent::Failed(_) => {
                    self.player.stop();
                    self.stopped = true;
                    let req_gen = self.current.as_ref().map_or(0, |(gen, _)| *gen);
                    // Мост С3: легаси `Player` отдаёт только текст ошибки;
                    // точная классификация `CaptureFailure` — следующий этап.
                    self.deps.events.emit(EngineEvent::OpenFailed {
                        req_gen,
                        err: OpenError::Capture(CaptureFailure::ReservationDenied { owner: None }),
                    });
                    self.refresh_transport();
                }
                ReservationEvent::Lost(_) => {
                    // `Player` уже освободил движок внутри себя (И-Р20).
                    self.stopped = true;
                    let req_gen = self.current.as_ref().map_or(0, |(gen, _)| *gen);
                    self.deps.events.emit(EngineEvent::OpenFailed {
                        req_gen,
                        err: OpenError::Capture(CaptureFailure::ReservationLost),
                    });
                    self.refresh_transport();
                }
            }
        }

        // §6.18 шаг 19 (ТЗ-87): мид-трековый отказ декодера после `Ready` —
        // `DecodeWorker` после `Failed` сам доходит до EOF (`shared.ended`),
        // поэтому без этой проверки трек выглядел бы штатно доигранным.
        // Это пропуск, а не конец трека — `session_failed` гасит `Ended` ниже.
        if let Some(e) = self.player.poll_decode_failure() {
            if let Some((req_gen, path)) = &self.current {
                let reason = match e {
                    // Кадр отказа недоступен дёшево на этой границе (мост С3,
                    // без `as`-приведений позиции): фиксируется 0, точная
                    // позиция — следующий этап.
                    FileError::Io(kind) => FileError::ReadDuringPlayback { at_frame: 0, kind },
                    other => other,
                };
                self.deps.events.emit(EngineEvent::Skipped {
                    req_gen: *req_gen,
                    path: Arc::clone(path),
                    reason: SkipReason::File(reason),
                });
            }
            self.session_failed = true;
        }

        if self.player.ended() {
            if !self.session_failed {
                if let Some((req_gen, _)) = &self.current {
                    self.deps.events.emit(EngineEvent::Ended { session: *req_gen });
                }
            }
            // Движок — единственный потребитель флага конца трека теперь
            // (легаси UI-слой флаг больше не читает).
            self.player.clear_end();
        }

        self.refresh_transport();

        let (_, pos, _) = self.player.snapshot();
        let now = self.deps.clock.now();
        let due = self.last_pos_at.is_none_or(|at| now.saturating_since(at) >= POSITION_INTERVAL);
        if due && self.last_pos != Some(pos) {
            self.last_pos = Some(pos);
            self.last_pos_at = Some(now);
            self.deps.events.emit(EngineEvent::Position { secs: pos });
        }
    }
}

/// Что умеет бэкенд на этой платформе (§2.3, ОВС-12, ТЗ-109, ТЗ-110).
/// Статичен для платформы: `ExclusiveBackend::caps()` не выполняет I/O,
/// поэтому UI знает доступность режимов синхронно (§2.8). Определён здесь,
/// а не в `audio/backend/mod.rs` (§2.3), т.к. этот шаг не вправе менять тот
/// файл — перенос в `audio/backend` целиком относится к более позднему этапу.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCaps {
    /// Доступен ли Exclusive-бэкенд на этой сборке. Временное соответствие
    /// старому exclusive-пути (ОВС-12): `true` только на Linux, т.к. других
    /// реализаций `ExclusiveBackend` пока нет (ТЗ-109, ТЗ-110).
    pub exclusive: bool,
}

impl BackendCaps {
    /// Возможности текущей сборки (ОВС-12, ТЗ-109, ТЗ-110).
    pub fn current() -> Self {
        Self { exclusive: cfg!(target_os = "linux") }
    }

    /// Доступность режима `mode` при данных возможностях бэкенда (ТЗ-109):
    /// Совместимый доступен всегда, Оптимальный/Строгий — только при
    /// `exclusive`.
    pub fn available(&self, mode: ModeKind) -> bool {
        mode == ModeKind::Compatible || self.exclusive
    }
}

/// Handle движка в UI (§2.8, ADR-01): хранит неблокирующий `Sender` и
/// `JoinHandle` для выхода (ТЗ-45).
pub struct EngineHandle {
    tx: mpsc::Sender<EngineCmd>,
    join: Option<JoinHandle<()>>,
}

impl EngineHandle {
    /// Запускает поток `apap-engine` через `deps.spawner` (ТЗ-88, ADR-20) и
    /// возвращает синхронно `BackendCaps` платформы (§2.8, ОВС-12).
    pub fn spawn(deps: EngineDeps) -> Result<(EngineHandle, BackendCaps), EngineFault> {
        Self::spawn_with_caps(deps, BackendCaps::current())
    }

    /// Как [`Self::spawn`], но с явными `BackendCaps` — для тестов
    /// недоступности режима без зависимости от платформы сборки (ТЗ-109,
    /// ТЗ-111).
    pub(crate) fn spawn_with_caps(deps: EngineDeps, caps: BackendCaps) -> Result<(EngineHandle, BackendCaps), EngineFault> {
        let spawner = Arc::clone(&deps.spawner);
        let (tx, rx) = mpsc::channel();
        let join = spawner.spawn(
            "apap-engine",
            Box::new(move || {
                let engine = Engine::new(deps, caps);
                engine.run(rx);
            }),
        )?;
        Ok((EngineHandle { tx, join: Some(join) }, caps))
    }

    /// Неблокирующая отправка команды (ADR-01). `false`, если движок уже
    /// завершился (получатель отключён).
    pub fn send(&self, cmd: EngineCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }

    /// Отдельный отправитель команд для владельцев вне `MusicApp` (ADR-01):
    /// не держит `JoinHandle`, поэтому его `Drop` не останавливает движок.
    pub fn sender(&self) -> EngineSender {
        EngineSender {
            tx: self.tx.clone(),
        }
    }

    /// Дожидается завершения потока движка. Берёт `JoinHandle` из `self`,
    /// чтобы `Drop` не пытался join повторно.
    pub fn join(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Неблокирующий отправитель команд движка (ADR-01): `Clone + Send`,
/// выход и `join` остаются за [`EngineHandle`].
#[derive(Clone)]
pub struct EngineSender {
    tx: mpsc::Sender<EngineCmd>,
}

impl EngineSender {
    /// Неблокирующая отправка команды (ADR-01). `false`, если движок уже
    /// завершился (получатель отключён).
    pub fn send(&self, cmd: EngineCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(EngineCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::audio::backend::catalog::FakeDeviceWatcher;
    use crate::audio::backend::shared::{fake_device, FakeSharedBackend};
    use crate::audio::backend::{BackendError, SharedDeviceId};
    use crate::audio::format::{ChannelLayout, Codec, Container, SampleRate, SourceFormat, SourceKind};
    use crate::audio::clock::MonotonicClock;
    use crate::engine::sink::VecSink;
    use crate::engine::source::FakeSource;
    use crate::engine::spawner::{FailingSpawner, StdSpawner};
    use crate::platform::fs::{FsPersistStore, MemStore};
    use std::path::PathBuf;

    fn pcm_format() -> SourceFormat {
        SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: crate::audio::format::BitDepth::new(16).expect("bits") },
        }
    }

    fn fake_deps(events: Box<dyn crate::engine::sink::EventSink>, spawner: Arc<dyn crate::engine::spawner::ThreadSpawner>) -> EngineDeps {
        fake_deps_with(events, spawner, Box::new(FakeSharedBackend::with_devices(Vec::new())), Box::new(FakeDeviceWatcher::new()))
    }

    fn fake_deps_with(
        events: Box<dyn crate::engine::sink::EventSink>,
        spawner: Arc<dyn crate::engine::spawner::ThreadSpawner>,
        shared: Box<dyn crate::audio::backend::shared::SharedBackend>,
        watcher: Box<dyn crate::audio::backend::catalog::DeviceWatcher>,
    ) -> EngineDeps {
        let mem = MemStore::new();
        EngineDeps {
            shared,
            watcher,
            sources: Arc::new(FakeSource::with_format(pcm_format())),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events,
            spawner,
        }
    }

    #[test]
    fn engine_spawn_and_shutdown() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));

        let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
        assert!(handle.send(EngineCmd::SetVolume(0.5)));
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::ShutdownComplete)));
    }

    #[test]
    fn engine_spawn_failure_reported() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink), Arc::new(FailingSpawner::new(1)));

        match EngineHandle::spawn(deps) {
            Err(EngineFault::SpawnFailed { what: "apap-engine" }) => {}
            Err(other) => panic!("expected SpawnFailed, got {other:?}"),
            Ok(_) => panic!("expected spawn to fail"),
        }
    }

    #[test]
    fn engine_startup_emits_devices_once() {
        let sink = VecSink::new();
        let shared = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        let deps =
            fake_deps_with(Box::new(sink.clone()), Arc::new(StdSpawner), Box::new(shared), Box::new(FakeDeviceWatcher::new()));

        let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        let devices: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Devices(catalog) => Some(catalog.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].shared.len(), 1);
        assert_eq!(devices[0].shared[0].id, SharedDeviceId::new("hw:0"));
    }

    #[test]
    fn engine_device_watcher_fire_unchanged_emits_no_duplicate() {
        let sink = VecSink::new();
        let shared = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        let watcher = FakeDeviceWatcher::new();
        let trigger = watcher.trigger_handle();
        let deps =
            fake_deps_with(Box::new(sink.clone()), Arc::new(StdSpawner), Box::new(shared), Box::new(watcher));

        let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
        trigger.fire();
        assert!(handle.send(EngineCmd::SetVolume(0.5)));
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        let devices_count = events.iter().filter(|e| matches!(e, EngineEvent::Devices(_))).count();
        assert_eq!(devices_count, 1);
    }

    #[test]
    fn engine_startup_device_error_emits_notice() {
        let sink = VecSink::new();
        let shared = FakeSharedBackend::with_error(BackendError::Unavailable("no host".to_string()));
        let deps =
            fake_deps_with(Box::new(sink.clone()), Arc::new(StdSpawner), Box::new(shared), Box::new(FakeDeviceWatcher::new()));

        let (handle, _caps) = EngineHandle::spawn(deps).expect("spawn ok");
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(
            e,
            EngineEvent::Notice(Notice::DevicesUnavailable(BackendError::Unavailable(msg))) if msg == "no host"
        )));
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::Devices(_))));
    }
}
