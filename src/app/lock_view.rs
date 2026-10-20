//! Чистый презентер замка устройства: `LockState` → вид для UI (ADR-06,
//! §2.6, §6.19, ТЗ-46, ТЗ-50, ТЗ-118…ТЗ-123, ТЗ-144). Модуль не знает о
//! Slint-свойствах, кроме самого перечисления `LockKind` — подключение
//! остального (`lock-text`/`lock-hint`, действия) к `mod.rs` выполняет
//! `MusicApp::apply_lock_view` (AM1.0-8.6 шаг 70).

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
    /// «Закрыть замок» — явный переход в закрытое ожидание захвата
    /// (ТЗ-118, §6.19, `LockInput::UserClose`).
    Close,
    /// «Открыть замок» — освободить захваченное устройство (§6.19,
    /// `LockInput::UserOpen`).
    Open,
    /// «Повторить захват» (ТЗ-46).
    RetryCapture,
    /// «Перейти в Совместимый режим» (ТЗ-46, ТЗ-144).
    SwitchToCompatible,
    /// «Выбрать устройство» (ТЗ-144).
    ChooseDevice,
}

/// Действия для состояния `Open`: «Закрыть замок» (§6.19 `UserClose`
/// разрешён из `Open`).
const OPEN_ACTIONS: &[LockAction] = &[LockAction::Close];

/// Действия для состояния `Held`: «Открыть замок» (§6.19 `UserOpen`
/// разрешён из `Held`).
const HELD_ACTIONS: &[LockAction] = &[LockAction::Open];

/// Действия для обычного отказа захвата: «Повторить захват» и «Перейти в
/// Совместимый режим» (ТЗ-46).
const FAILED_ACTIONS: &[LockAction] = &[LockAction::RetryCapture, LockAction::SwitchToCompatible];

/// Действия для «устройство отключено»: кроме обычного отказа — «Открыть
/// замок» (§6.19: `UserOpen` разрешён и из `Failed(DeviceDisconnected)`,
/// так как это ожидаемый терминал при физическом отключении устройства).
const DEVICE_DISCONNECTED_ACTIONS: &[LockAction] =
    &[LockAction::RetryCapture, LockAction::Open, LockAction::SwitchToCompatible];

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
        CaptureFailure::BusyOutsideProtocol { .. } => {
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
            actions: OPEN_ACTIONS,
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
            actions: HELD_ACTIONS,
        },
        LockState::Failed(CaptureFailure::NoDeviceSelected) => LockView {
            kind: LockKind::Failed,
            text: "устройство не выбрано".to_string(),
            hint: "устройство не выбрано".to_string(),
            actions: NO_DEVICE_ACTIONS,
        },
        LockState::Failed(f) => {
            let reason = failure_label(f);
            // `DeviceDisconnected` — единственный отказ, из которого §6.19
            // дополнительно разрешает `UserOpen` (ожидаемый терминал при
            // физическом отключении устройства, ТЗ-120).
            let actions = if matches!(f, CaptureFailure::DeviceDisconnected) {
                DEVICE_DISCONNECTED_ACTIONS
            } else {
                FAILED_ACTIONS
            };
            LockView {
                kind: LockKind::Failed,
                text: format!("не удалось захватить: {reason}"),
                hint: reason,
                actions,
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
        let v = lock_view(&LockState::Failed(CaptureFailure::BusyOutsideProtocol { owner: None }));
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

    /// ТЗ-118, §6.19: из `Open` доступно только «Закрыть замок»
    /// (`UserClose` разрешён из `Open`).
    #[test]
    fn open_action_is_close() {
        let v = lock_view(&LockState::Open);
        assert_eq!(v.actions, OPEN_ACTIONS);
        assert_eq!(v.actions, &[LockAction::Close]);
    }

    /// §6.19: из `Held` доступно только «Открыть замок» (`UserOpen`
    /// разрешён из `Held`).
    #[test]
    fn held_action_is_open() {
        let device = "hw:CARD=X,DEV=0".parse().expect("валидный HwDeviceId");
        let v = lock_view(&LockState::Held { info: ClaimInfo { device } });
        assert_eq!(v.actions, HELD_ACTIONS);
        assert_eq!(v.actions, &[LockAction::Open]);
    }

    /// ТЗ-120, §6.19: `Failed(DeviceDisconnected)` дополнительно разрешает
    /// «Открыть замок» рядом с «Повторить захват»/«Перейти в Совместимый
    /// режим»; текст отказа не меняется.
    #[test]
    fn device_disconnected_adds_open_action() {
        let v = lock_view(&LockState::Failed(CaptureFailure::DeviceDisconnected));
        assert_eq!(v.actions, DEVICE_DISCONNECTED_ACTIONS);
        assert!(v.text.contains("устройство отключено"));
    }
}
