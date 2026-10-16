//! Чистый презентер бейджа тракта и кнопки «Тест»: `BadgeState`/`TestBadge`
//! → вид для Slint (ADR-06, ТЗ-52…ТЗ-58, ТЗ-61, ТЗ-62, ТЗ-65, ОВ-14, ОВ-20,
//! ОВС-18). Модуль не знает о Slint-свойствах — подключение к
//! `ui_manager.rs` будет следующим шагом, поэтому пока не вызывается.
#![allow(dead_code)]

use music_player_rs::audio::test_run::{test_availability, IncompleteWhy, TestAvailability, TestBadge, TestBlocker};
use music_player_rs::audio::verdict::{BadgeState, ExclusiveBadge, NbpReason, NbpSummary, SharedBadge};
use music_player_rs::settings::ModeKind;

use super::BadgeKind;

/// Короткая подпись причины «Не bit-perfect» для бейджа (ТЗ-53).
pub(super) fn nbp_reason_label(r: NbpReason) -> &'static str {
    match r {
        NbpReason::TestFailed => "тест не пройден",
        NbpReason::DriverParamsUnconfirmed => "параметры драйвера не подтверждены",
        NbpReason::DsdToPcm => "DSD→PCM",
        NbpReason::Resample => "ресемплинг",
        NbpReason::Downmix => "downmix",
        NbpReason::Truncation => "усечение разрядности",
        NbpReason::FloatToInt => "квантование float-источника",
        NbpReason::Int32ToFloat => "int32 → float",
        NbpReason::Mute => "mute",
        NbpReason::Volume => "громкость",
        NbpReason::Dither => "дизеринг",
    }
}

/// Текст «Не bit-perfect: <причина> [+N]» (ТЗ-53).
fn nbp_text(s: &NbpSummary) -> String {
    let mut text = format!("Не bit-perfect: {}", nbp_reason_label(s.primary));
    if s.more > 0 {
        text.push_str(&format!(" +{}", s.more));
    }
    text
}

/// Вид бейджа тракта для Slint: `kind` выбирает цвет, `text` — подпись
/// (ТЗ-52, ТЗ-53, ТЗ-54, ТЗ-56, ОВС-18, ADR-06).
#[derive(Clone, PartialEq, Debug)]
pub(super) struct BadgeView {
    pub kind: BadgeKind,
    pub text: String,
}

/// Строит `BadgeView` из `BadgeState` (ADR-06).
pub(super) fn badge_view(b: &BadgeState) -> BadgeView {
    match b {
        BadgeState::NoStream => BadgeView { kind: BadgeKind::NoStream, text: "Нет потока".to_owned() },
        BadgeState::Shared(SharedBadge::Clean) => {
            BadgeView { kind: BadgeKind::SharedClean, text: "Shared: без изменений до системы".to_owned() }
        }
        BadgeState::Shared(SharedBadge::NotBitPerfect(s)) => {
            BadgeView { kind: BadgeKind::NotBitPerfect, text: nbp_text(s) }
        }
        BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(s)) => {
            BadgeView { kind: BadgeKind::NotBitPerfect, text: nbp_text(s) }
        }
        BadgeState::Exclusive(ExclusiveBadge::BitPerfect(_)) => {
            BadgeView { kind: BadgeKind::BitPerfect, text: "Bit-perfect".to_owned() }
        }
        BadgeState::Exclusive(ExclusiveBadge::Muted) => BadgeView { kind: BadgeKind::Muted, text: "Mute".to_owned() },
    }
}

/// Короткая подпись причины незавершённого теста (ТЗ-67).
fn incomplete_why_label(why: IncompleteWhy) -> &'static str {
    match why {
        IncompleteWhy::Underrun => "underrun",
        IncompleteWhy::Xrun => "xrun",
        IncompleteWhy::MirrorOverflow => "переполнение зеркала",
        IncompleteWhy::Seek => "перемотка",
        IncompleteWhy::TrackChanged => "смена трека",
        IncompleteWhy::ModeChanged => "смена режима",
        IncompleteWhy::DeviceChanged => "смена устройства",
        IncompleteWhy::LockOpened => "открыт замок",
    }
}

/// Подсказка блокировки кнопки «Тест» (ТЗ-61, ТЗ-62, ТЗ-65, ОВ-14, ОВ-20).
fn blocker_hint(blocker: &TestBlocker) -> String {
    match blocker {
        TestBlocker::SharedMode => "Проверка недоступна в Shared".to_owned(),
        TestBlocker::NoStream => "Запустите воспроизведение".to_owned(),
        TestBlocker::Lossy(r) => format!("Мешает тесту: {} — см. «Путь сигнала»", nbp_reason_label(*r)),
        TestBlocker::DriverParamsUnconfirmed => {
            "Мешает тесту: параметры драйвера не подтверждены — см. «Путь сигнала»".to_owned()
        }
        TestBlocker::Muted => "Выключите mute".to_owned(),
    }
}

/// Вид кнопки-бейджа «Тест» для Slint (ТЗ-61, ТЗ-62, ТЗ-65, ОВ-14, ОВ-20).
#[derive(Clone, PartialEq, Debug)]
pub(super) struct TestView {
    pub visible: bool,
    pub enabled: bool,
    pub text: String,
    pub hint: String,
}

/// Строит `TestView` из бейджа тракта, режима и результата «Теста»
/// (ТЗ-61, ТЗ-62, ТЗ-65, ОВ-14, ОВ-20, ОВС-18).
pub(super) fn test_view(badge: &BadgeState, mode: ModeKind, test: Option<&TestBadge>) -> TestView {
    let availability = test_availability(badge, mode);
    let enabled = matches!(availability, TestAvailability::Enabled);
    let text = match test {
        Some(TestBadge::Passed { .. }) => "Тест ✓".to_owned(),
        Some(TestBadge::Failed { .. }) => "Тест ✕".to_owned(),
        Some(TestBadge::Unknown(_)) | None => "Тест ?".to_owned(),
        Some(TestBadge::Running { elapsed, max }) => {
            format!("Тест {} / {} с", elapsed.as_secs(), max.as_secs())
        }
    };
    let hint = match &availability {
        TestAvailability::Disabled(blocker) => blocker_hint(blocker),
        TestAvailability::Enabled => match test {
            Some(TestBadge::Unknown(Some(why))) => {
                format!("Последний тест не завершён: {}", incomplete_why_label(*why))
            }
            _ => "Проверить bit-perfect тракт".to_owned(),
        },
    };
    TestView { visible: true, enabled, text, hint }
}

#[cfg(test)]
mod tests {
    use music_player_rs::audio::verdict::NbpReason;
    use music_player_rs::settings::ModeKind;

    use super::*;

    fn nbp(primary: NbpReason, more: u8) -> NbpSummary {
        NbpSummary { primary, more }
    }

    /// ТЗ-54: Shared(Clean) и Shared(NotBitPerfect{Volume}) без «+N».
    #[test]
    fn badge_view_shared_clean_and_volume() {
        let clean = badge_view(&BadgeState::Shared(SharedBadge::Clean));
        assert_eq!(clean.kind, BadgeKind::SharedClean);
        assert_eq!(clean.text, "Shared: без изменений до системы");

        let vol = badge_view(&BadgeState::Shared(SharedBadge::NotBitPerfect(nbp(NbpReason::Volume, 0))));
        assert_eq!(vol.kind, BadgeKind::NotBitPerfect);
        assert_eq!(vol.text, "Не bit-perfect: громкость");
    }

    /// ТЗ-53: «+N» добавляется только при `more > 0`.
    #[test]
    fn badge_view_plus_n() {
        let v = badge_view(&BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(nbp(NbpReason::Resample, 1))));
        assert_eq!(v.kind, BadgeKind::NotBitPerfect);
        assert_eq!(v.text, "Не bit-perfect: ресемплинг +1");
    }

    /// ТЗ-56, ОВС-18: «Нет потока» и «Mute».
    #[test]
    fn badge_view_no_stream_and_muted() {
        let no_stream = badge_view(&BadgeState::NoStream);
        assert_eq!(no_stream.kind, BadgeKind::NoStream);
        assert_eq!(no_stream.text, "Нет потока");

        let muted = badge_view(&BadgeState::Exclusive(ExclusiveBadge::Muted));
        assert_eq!(muted.kind, BadgeKind::Muted);
        assert_eq!(muted.text, "Mute");
    }

    /// ТЗ-62: в Совместимом режиме «Тест» всегда неактивен с подсказкой Shared.
    #[test]
    fn test_view_compatible_disabled_with_shared_hint() {
        for badge in [
            BadgeState::NoStream,
            BadgeState::Shared(SharedBadge::Clean),
            BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(nbp(NbpReason::Volume, 0))),
        ] {
            let v = test_view(&badge, ModeKind::Compatible, None);
            assert!(v.visible);
            assert!(!v.enabled);
            assert_eq!(v.hint, "Проверка недоступна в Shared");
        }
    }

    /// ОВ-20: причина с потерями называется в подсказке и ссылается на «Путь сигнала».
    #[test]
    fn test_view_lossy_hint_names_reason() {
        let badge = BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(nbp(NbpReason::Volume, 0)));
        let v = test_view(&badge, ModeKind::Optimal, None);
        assert!(!v.enabled);
        assert!(v.hint.contains("громкость"));
        assert!(v.hint.contains("Путь сигнала"));
    }
}
