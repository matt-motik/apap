//! Чистый презентер замка устройства: `LockState` → вид для UI (ADR-06,
//! §2.6, §6.19, ТЗ-46, ТЗ-50, ТЗ-118…ТЗ-123, ТЗ-144). Модуль не знает о
//! Slint-свойствах, кроме самого перечисления `LockKind` — подключение
//! остального (`lock-text`/`lock-hint`, действия) к `ui_manager.rs` будет
//! следующим шагом, поэтому пока не вызывается.
#![allow(dead_code)]

use music_player_rs::audio::error::CaptureFailure;
use music_player_rs::engine::lock::LockState;

#[cfg(test)]
use music_player_rs::audio::backend::exclusive::ClaimInfo;

// Вид замка для UI (§2.6): открыт/захват/закрыт/недоступен в Совместимом
// режиме/не удалось захватить. Перечисление сгенерировано из
// `ui/top_panel.slint` (`export enum LockKind`), варианты в CamelCase.
use super::LockKind;

/// Действие пользователя, доступное из вида замка (ТЗ-46, ТЗ-144).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LockAction {
    /// «Повторить захват» (ТЗ-46).
    RetryCapture,
    /// «Перейти в Совместимый режим» (ТЗ-46, ТЗ-144).
    SwitchToCompatible,
    /// «Выбрать устройство» (ТЗ-144).
    ChooseDevice,
}

/// Действия для обычного отказа захвата: «Повторить захват» и «Перейти в
/// Совместимый режим» (ТЗ-46).
const FAILED_ACTIONS: &[LockAction] = &[LockAction::RetryCapture, LockAction::SwitchToCompatible];

/// Действия для «устройство не выбрано»: «Выбрать устройство» и «Перейти в
/// Совместимый режим» (ТЗ-144, порядок как в ТЗ).
const NO_DEVICE_ACTIONS: &[LockAction] = &[LockAction::ChooseDevice, LockAction::SwitchToCompatible];

/// Нет действий, доступных пользователю из вида замка в этом состоянии.
const NO_ACTIONS: &[LockAction] = &[];

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
        CaptureFailure::BusyOutsideProtocol => {
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

/// Вид замка: вид, короткий текст, подсказка и доступные действия (§6.19).
#[derive(Clone, PartialEq, Debug)]
pub(super) struct LockView {
    pub kind: LockKind,
    pub text: String,
    pub hint: String,
    pub actions: &'static [LockAction],
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
            actions: NO_ACTIONS,
        },
        LockState::Open => LockView {
            kind: LockKind::Open,
            text: "открыт".to_string(),
            hint: String::new(),
            actions: NO_ACTIONS,
        },
        LockState::Acquiring { .. } => LockView {
            kind: LockKind::Acquiring,
            text: "захват…".to_string(),
            hint: String::new(),
            actions: NO_ACTIONS,
        },
        LockState::Held { .. } => LockView {
            kind: LockKind::Held,
            text: "закрыт".to_string(),
            hint: HELD_HINT.to_string(),
            actions: NO_ACTIONS,
        },
        LockState::Failed(CaptureFailure::NoDeviceSelected) => LockView {
            kind: LockKind::Failed,
            text: "устройство не выбрано".to_string(),
            hint: "устройство не выбрано".to_string(),
            actions: NO_DEVICE_ACTIONS,
        },
        LockState::Failed(f) => {
            let reason = failure_label(f);
            LockView {
                kind: LockKind::Failed,
                text: format!("не удалось захватить: {reason}"),
                hint: reason,
                actions: FAILED_ACTIONS,
            }
        }
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
        assert_eq!(v.kind, LockKind::Failed);
        assert!(v.text.contains("Spotify"));
        assert!(v.hint.contains("Spotify"));
        assert!(!v.text.contains("не найдено"));
    }

    /// ТЗ-122: «занято вне протокола» вместо «не найдено».
    #[test]
    fn busy_outside_protocol_not_not_found() {
        let v = lock_view(&LockState::Failed(CaptureFailure::BusyOutsideProtocol));
        assert!(v.text.contains("занято"));
        assert!(!v.text.contains("не найдено"));
    }

    /// ТЗ-144: «устройство не выбрано» — отдельный текст и действия
    /// «Выбрать устройство» / «Перейти в Совместимый режим».
    #[test]
    fn no_device_selected_text_and_actions() {
        let v = lock_view(&LockState::Failed(CaptureFailure::NoDeviceSelected));
        assert_eq!(v.text, "устройство не выбрано");
        assert_eq!(v.actions, NO_DEVICE_ACTIONS);
    }

    /// ТЗ-46: прочие отказы захвата — «Повторить захват» и «Перейти в
    /// Совместимый режим», errno виден в причине (`DriverRefused`).
    #[test]
    fn other_failed_actions_and_errno() {
        let v = lock_view(&LockState::Failed(CaptureFailure::DriverRefused { errno: 5 }));
        assert_eq!(v.actions, FAILED_ACTIONS);
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
}
