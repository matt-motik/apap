//! Чистый презентер замка устройства: `LockState` → вид для кнопки-значка
//! (ADR-06, §2.6, §6.19, ТЗ-44, ТЗ-46, ТЗ-50, ТЗ-118…ТЗ-123, ТЗ-144). Модуль
//! не знает о Slint-свойствах, кроме самого перечисления `LockKind` —
//! подключение остального (`lock-text`/`lock-hint`) к `mod.rs` выполняет
//! `MusicApp::apply_lock_view` (AM1.0-8.6 шаг 70, AM1.0-B14 шаг 6).

use music_player_rs::audio::error::CaptureFailure;
use music_player_rs::engine::lock::LockState;

#[cfg(test)]
use music_player_rs::audio::backend::exclusive::ClaimInfo;
#[cfg(test)]
use music_player_rs::audio::clock::ClockInstant;

// Вид замка для UI (§2.6): открыт/захват/закрыт/недоступен в Совместимом
// режиме/не удалось захватить. Перечисление сгенерировано из
// `ui/top_panel.slint` (`export enum LockKind`), варианты в CamelCase.
use super::LockKind;

/// Короткая причина отказа захвата (ТЗ-46, канонический список из критерия
/// приёмки). `ReservationDenied`/`DriverRefused` подставляют владельца/errno
/// (ТЗ-119) — это и закрывает AM1.0-B2: вместо «устройство не найдено»
/// показывается настоящая причина.
pub(super) fn failure_label(f: &CaptureFailure) -> String {
    match f {
        CaptureFailure::ReservationDenied { owner: Some(owner) } => {
            format!("резервирование отклонено ({owner})")
        }
        CaptureFailure::ReservationDenied { owner: None } => "резервирование отклонено".to_string(),
        CaptureFailure::OwnerNotResponding => "владелец резервирования не отвечает".to_string(),
        CaptureFailure::NoSessionBus => "нет сессионной шины D-Bus".to_string(),
        CaptureFailure::BusyOutsideProtocol { owner: Some(owner) } => {
            format!("устройство занято приложением {} (PID {})", owner.name, owner.pid)
        }
        CaptureFailure::BusyOutsideProtocol { owner: None } => {
            "устройство занято приложением вне протокола резервирования".to_string()
        }
        CaptureFailure::DriverRefused { errno } => format!("отказ драйвера (errno {errno})"),
        CaptureFailure::LostOnReopen => "перехвачено при переоткрытии".to_string(),
        CaptureFailure::DeviceDisconnected => "устройство отключено".to_string(),
        CaptureFailure::ReservationLost => "резервирование перехвачено".to_string(),
        CaptureFailure::PlatformUnsupported => "платформа не поддерживается".to_string(),
        CaptureFailure::NoDeviceSelected => "устройство не выбрано".to_string(),
    }
}

/// Подсказка замка для отказа захвата (ТЗ-122): по умолчанию совпадает с
/// `failure_label`, но для «занято вне протокола» с известным владельцем
/// заменяется на указание закрыть приложение-владельца. Этот же текст идёт
/// в тело окна ошибки замка (AM1.0-B14 шаг 5, `mod.rs::lock_failure_message`).
pub(super) fn failure_hint(f: &CaptureFailure) -> String {
    match f {
        CaptureFailure::BusyOutsideProtocol { owner: Some(owner) } => format!(
            "карту открыло напрямую приложение {}, минуя резервирование PipeWire. \
Закройте его и нажмите «Повторить захват»",
            owner.name
        ),
        _ => failure_label(f),
    }
}

/// Вид замка: вид (значок кнопки, ТЗ-44), короткий текст и подсказка
/// (ТЗ-119, ТЗ-121) — кнопка-значок (AM1.0-B14 шаг 6) выбирает клик
/// (`close`/`open`) по `kind` самостоятельно: действие пользователя больше
/// не отдельное поле (набор кнопок окна ошибки по причине — отдельная,
/// независимая функция `lock_failure_actions`, `src/core/messages.rs`).
#[derive(Clone, PartialEq, Debug)]
pub(super) struct LockView {
    pub kind: LockKind,
    pub text: String,
    pub hint: String,
}

/// Подсказка состояния `Held` (ТЗ-121): звук чужих приложений, адресованный
/// захваченному устройству, система перенаправляет на другое — он может
/// прозвучать из другого выхода.
const HELD_HINT: &str = "пока устройство захвачено, звук других приложений, \
адресованный этому устройству, система перенаправляет на другое устройство — \
он может прозвучать из другого выхода";

/// Подсказка состояния `NotApplicable` (ТЗ-50): замок не нужен в
/// Совместимом режиме.
const NOT_APPLICABLE_HINT: &str = "в Совместимом режиме устройство не захватывается";

/// Строит вид замка из состояния автомата (§6.19, ADR-06).
pub(super) fn lock_view(state: &LockState) -> LockView {
    match state {
        LockState::NotApplicable => LockView {
            kind: LockKind::NotApplicable,
            text: String::new(),
            hint: NOT_APPLICABLE_HINT.to_string(),
        },
        LockState::Open => LockView {
            kind: LockKind::Open,
            text: "открыт".to_string(),
            hint: String::new(),
        },
        LockState::Acquiring { .. } => LockView {
            kind: LockKind::Acquiring,
            text: "захват…".to_string(),
            hint: String::new(),
        },
        LockState::Held { .. } => LockView {
            kind: LockKind::Held,
            text: "закрыт".to_string(),
            hint: HELD_HINT.to_string(),
        },
        LockState::Failed(CaptureFailure::NoDeviceSelected) => LockView {
            // Решение пользователя 2026-10-23 (ТЗ-46, ТЗ-144): замок
            // остаётся визуально «открыт» (клик снова начинает захват) —
            // причину отказа и действия несёт отдельное окно ошибки
            // (AM1.0-B14 шаг 5), а не значок замка.
            kind: LockKind::Open,
            text: "устройство не выбрано".to_string(),
            hint: "устройство не выбрано".to_string(),
        },
        LockState::Failed(f) => {
            let reason = failure_label(f);
            LockView {
                // См. комментарий у `NoDeviceSelected` выше — тот же
                // редизайн (ТЗ-46): клик по кнопке-значку снова начинает
                // захват независимо от причины (`DeviceDisconnected` и
                // прочие отказы — тот же `close()`, §6.19 `UserClose`).
                kind: LockKind::Open,
                text: format!("не удалось захватить: {reason}"),
                hint: failure_hint(f),
            }
        }
    }
}

/// Решение «что сделать с окном ошибки замка» на переходе `prev -> next`
/// автомата `LockState` (ТЗ-46, ТЗ-126, AM1.0-B14 шаг 5). Чистая функция, не
/// знающая о `MessageCenter`: `MusicApp::apply_lock_failure_message` в
/// `mod.rs` вызывает её на каждом принятом `EngineEvent::Lock` ровно
/// однажды (движок уже не шлёт событие, когда состояние не изменилось —
/// `src/engine/run.rs`, `changed = state != self.lock` — так что «тот же
/// отказ, что уже показан» эта функция видит только при редком прямом
/// переходе `Failed(a) -> Failed(b)` без промежуточного `Acquiring`,
/// например повтор захвата без выбранного устройства, `lock_step`
/// `acquire_or_fail`).
#[derive(Clone, PartialEq, Debug)]
pub(super) enum LockFailureEffect {
    /// Переход в `Failed` из состояния, не являвшегося отказом: показать
    /// новое окно ошибки с этой причиной.
    Show(CaptureFailure),
    /// `Failed(a) -> Failed(b)` с разной причиной: заменить содержимое
    /// показанного (или ещё не показанного) окна ошибки, не показывая
    /// промежуточно пустой экран.
    Replace(CaptureFailure),
    /// Уход из `Failed` в любое другое состояние — автоматический повторный
    /// захват вернувшегося устройства (ТЗ-126) или действие пользователя:
    /// закрыть окно ошибки, если оно ещё открыто.
    Dismiss,
    /// Переход не касается окна ошибки замка: не отказ, либо тот же отказ,
    /// что уже показан.
    None,
}

pub(super) fn lock_failure_transition(prev: &LockState, next: &LockState) -> LockFailureEffect {
    match (prev, next) {
        (LockState::Failed(_), LockState::Failed(reason)) => {
            if prev == next {
                LockFailureEffect::None
            } else {
                LockFailureEffect::Replace(reason.clone())
            }
        }
        (LockState::Failed(_), _) => LockFailureEffect::Dismiss,
        (_, LockState::Failed(reason)) => LockFailureEffect::Show(reason.clone()),
        _ => LockFailureEffect::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AM1.0-B2, ТЗ-119: владелец резервирования называется в тексте и
    /// подсказке вместо прежнего неверного «устройство не найдено».
    #[test]
    fn reservation_denied_with_owner_names_owner() {
        let f = CaptureFailure::ReservationDenied { owner: Some("Spotify".to_string()) };
        let v = lock_view(&LockState::Failed(f));
        // ТЗ-46 (решение 2026-10-23): замок визуально остаётся «открыт» —
        // причину несёт окно ошибки, не значок замка.
        assert_eq!(v.kind, LockKind::Open);
        assert!(v.text.contains("Spotify"));
        assert!(v.hint.contains("Spotify"));
        assert!(!v.text.contains("не найдено"));
    }

    /// ТЗ-122: «занято вне протокола» вместо «не найдено»; владелец
    /// неизвестен — текст и подсказка не меняются.
    #[test]
    fn busy_outside_protocol_not_not_found() {
        let v = lock_view(&LockState::Failed(CaptureFailure::BusyOutsideProtocol { owner: None }));
        assert!(v.text.contains("занято"));
        assert!(!v.text.contains("не найдено"));
        assert_eq!(v.text, "не удалось захватить: устройство занято приложением вне протокола резервирования");
        assert_eq!(v.hint, "устройство занято приложением вне протокола резервирования");
    }

    /// ТЗ-122: владелец известен — текст и подсказка называют процесс и PID,
    /// подсказка советует закрыть приложение-владельца.
    #[test]
    fn lock_view_busy_owner_text() {
        let owner = music_player_rs::audio::error::BusyOwner { name: "aplay".to_string(), pid: 4242 };
        let v = lock_view(&LockState::Failed(CaptureFailure::BusyOutsideProtocol { owner: Some(owner) }));
        assert_eq!(v.text, "не удалось захватить: устройство занято приложением aplay (PID 4242)");
        assert_eq!(
            v.hint,
            "карту открыло напрямую приложение aplay, минуя резервирование PipeWire. \
Закройте его и нажмите «Повторить захват»"
        );
    }

    /// ТЗ-144: «устройство не выбрано» — отдельный текст; кнопка-значок
    /// остаётся «открыт» (набор кнопок окна ошибки — `lock_failure_actions`,
    /// независимо от этого модуля).
    #[test]
    fn no_device_selected_text_and_kind() {
        let v = lock_view(&LockState::Failed(CaptureFailure::NoDeviceSelected));
        assert_eq!(v.text, "устройство не выбрано");
        assert_eq!(v.kind, LockKind::Open);
    }

    /// ТЗ-46: прочие отказы захвата — кнопка-значок остаётся «открыт»
    /// (клик снова начинает захват), errno виден в причине (`DriverRefused`).
    #[test]
    fn other_failed_kind_and_errno() {
        let v = lock_view(&LockState::Failed(CaptureFailure::DriverRefused { errno: 5 }));
        assert_eq!(v.kind, LockKind::Open);
        assert!(v.text.contains('5'));
    }

    /// ТЗ-50 (`NotApplicable`) и ТЗ-121 (`Held`): подсказки замка.
    #[test]
    fn not_applicable_and_held_hints() {
        let na = lock_view(&LockState::NotApplicable);
        assert_eq!(na.hint, "в Совместимом режиме устройство не захватывается");

        let device = "hw:CARD=X,DEV=0".parse().expect("валидный HwDeviceId");
        let held = lock_view(&LockState::Held { info: ClaimInfo { device } });
        assert!(held.hint.contains("перенаправляет"));
    }

    /// ТЗ-118, §6.19: из `Open` кнопка-значок кликом вызывает `close()`
    /// (`UserClose` разрешён из `Open`, AM1.0-B14 шаг 6 — выбор по `kind` в
    /// Slint, не отдельное поле).
    #[test]
    fn open_state_kind_is_open() {
        let v = lock_view(&LockState::Open);
        assert_eq!(v.kind, LockKind::Open);
        assert_eq!(v.text, "открыт");
    }

    /// §6.19: из `Held` кнопка-значок кликом вызывает `open()` (`UserOpen`
    /// разрешён из `Held`).
    #[test]
    fn held_state_kind_is_held() {
        let device = "hw:CARD=X,DEV=0".parse().expect("валидный HwDeviceId");
        let v = lock_view(&LockState::Held { info: ClaimInfo { device } });
        assert_eq!(v.kind, LockKind::Held);
    }

    /// ТЗ-46: `Failed(DeviceDisconnected)` — тот же вид «открыт», что и
    /// прочие отказы (клик снова начинает захват); текст называет причину.
    #[test]
    fn device_disconnected_kind_is_open() {
        let v = lock_view(&LockState::Failed(CaptureFailure::DeviceDisconnected));
        assert_eq!(v.kind, LockKind::Open);
        assert!(v.text.contains("устройство отключено"));
    }

    /// `lock_failure_transition` (AM1.0-B14 шаг 5): переход в `Failed` из
    /// `Acquiring` — показать новое окно ошибки.
    #[test]
    fn transition_into_failed_shows() {
        let acquiring = LockState::Acquiring { started: ClockInstant::START };
        let failed = LockState::Failed(CaptureFailure::DeviceDisconnected);
        assert_eq!(
            lock_failure_transition(&acquiring, &failed),
            LockFailureEffect::Show(CaptureFailure::DeviceDisconnected)
        );
    }

    /// Та же причина отказа повторно (движок в норме не шлёт такое событие,
    /// но функция тотальна) — без действия.
    #[test]
    fn transition_same_failure_is_none() {
        let failed = LockState::Failed(CaptureFailure::OwnerNotResponding);
        assert_eq!(lock_failure_transition(&failed, &failed), LockFailureEffect::None);
    }

    /// `Failed(a) -> Failed(b)` с разной причиной (например, `RetryCapture`
    /// без выбранного устройства, `lock_step::acquire_or_fail`) — заменить
    /// содержимое окна, а не закрыть и снова открыть.
    #[test]
    fn transition_different_failure_replaces() {
        let a = LockState::Failed(CaptureFailure::DriverRefused { errno: 5 });
        let b = LockState::Failed(CaptureFailure::NoDeviceSelected);
        assert_eq!(lock_failure_transition(&a, &b), LockFailureEffect::Replace(CaptureFailure::NoDeviceSelected));
    }

    /// Уход из `Failed` в `Acquiring` (автозахват вернувшегося устройства,
    /// ТЗ-126) закрывает окно ошибки.
    #[test]
    fn transition_out_of_failed_dismisses() {
        let failed = LockState::Failed(CaptureFailure::DeviceDisconnected);
        let acquiring = LockState::Acquiring { started: ClockInstant::START };
        assert_eq!(lock_failure_transition(&failed, &acquiring), LockFailureEffect::Dismiss);
    }

    /// Переход, не касающийся `Failed` ни с одной стороны — без действия.
    #[test]
    fn transition_unrelated_to_failed_is_none() {
        let device = "hw:CARD=X,DEV=0".parse().expect("валидный HwDeviceId");
        let held = LockState::Held { info: ClaimInfo { device } };
        assert_eq!(lock_failure_transition(&LockState::Open, &held), LockFailureEffect::None);
    }
}
