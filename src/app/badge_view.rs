//! Чистый презентер бейджа тракта и кнопки «Тест»: `BadgeState`/`TestBadge`
//! → вид для Slint (ADR-06, ТЗ-52…ТЗ-58, ТЗ-61, ТЗ-62, ТЗ-65, ОВ-14, ОВ-20,
//! ОВС-18). Модуль не знает о Slint-свойствах — подключение к
//! `ui_manager.rs` будет следующим шагом, поэтому пока не вызывается.
#![allow(dead_code)]

use music_player_rs::audio::test_run::{test_availability, TestAvailability, TestBadge};
use music_player_rs::audio::verdict::{BadgeState, ExclusiveBadge, NbpSummary, SharedBadge};
use music_player_rs::i18n;
use music_player_rs::settings::ModeKind;

use super::BadgeKind;

/// Текст «Не bit-perfect: <причина> [+N]» ([`i18n::badge_not_bit_perfect`],
/// ТЗ-53, T1.0-B2).
fn nbp_text(s: &NbpSummary) -> String {
    i18n::badge_not_bit_perfect(s.primary, s.more)
}

/// Вид бейджа тракта для Slint: `kind` выбирает цвет, `text` — подпись
/// (ТЗ-52, ТЗ-53, ТЗ-54, ТЗ-56, ОВС-18, ADR-06).
#[derive(Clone, PartialEq, Debug)]
pub(super) struct BadgeView {
    pub kind: BadgeKind,
    pub text: String,
}

/// Строит `BadgeView` из `BadgeState` (ADR-06); тексты — `src/i18n/`
/// (T1.0-B2).
pub(super) fn badge_view(b: &BadgeState) -> BadgeView {
    match b {
        BadgeState::NoStream => BadgeView { kind: BadgeKind::NoStream, text: i18n::BADGE_NO_STREAM.to_owned() },
        BadgeState::Shared(SharedBadge::Clean) => {
            BadgeView { kind: BadgeKind::SharedClean, text: i18n::BADGE_SHARED_CLEAN.to_owned() }
        }
        BadgeState::Shared(SharedBadge::NotBitPerfect(s)) => {
            BadgeView { kind: BadgeKind::NotBitPerfect, text: nbp_text(s) }
        }
        BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(s)) => {
            BadgeView { kind: BadgeKind::NotBitPerfect, text: nbp_text(s) }
        }
        BadgeState::Exclusive(ExclusiveBadge::BitPerfect(_)) => {
            BadgeView { kind: BadgeKind::BitPerfect, text: i18n::BADGE_BIT_PERFECT.to_owned() }
        }
        BadgeState::Exclusive(ExclusiveBadge::Muted) => {
            BadgeView { kind: BadgeKind::Muted, text: i18n::BADGE_MUTED.to_owned() }
        }
    }
}

/// Вид кнопки-бейджа «Тест» для Slint (ТЗ-61, ТЗ-62, ТЗ-65, ОВ-14, ОВ-20);
/// тексты и подсказки — `src/i18n/` (T1.0-B2).
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
        Some(TestBadge::Passed { .. }) => i18n::TEST_BADGE_PASSED.to_owned(),
        Some(TestBadge::Failed { .. }) => i18n::TEST_BADGE_FAILED.to_owned(),
        Some(TestBadge::Unknown(_)) | None => i18n::TEST_BADGE_UNKNOWN.to_owned(),
        Some(TestBadge::Running { elapsed, max }) => i18n::test_badge_running(elapsed.as_secs(), max.as_secs()),
    };
    let hint = match &availability {
        TestAvailability::Disabled(blocker) => i18n::test_blocker_hint(blocker),
        TestAvailability::Enabled => match test {
            Some(TestBadge::Unknown(Some(why))) => i18n::test_hint_incomplete(*why),
            _ => i18n::TEST_HINT_ENABLED.to_owned(),
        },
    };
    TestView { visible: true, enabled, text, hint }
}

/// Состояние «устарел ли вид бейджа тракта» (И-Р13, ТЗ-52, ТЗ-61, ТЗ-62):
/// хранит последний принятый бейдж и активный режим — предмет вида бейджа
/// и кнопки «Тест» (ADR-06); вызывающая сторона (`MusicApp`) взводит
/// пересчёт через `set_badge`/`set_mode`, снимает его только `take_view`.
pub(super) struct BadgeViewState {
    badge: BadgeState,
    mode: ModeKind,
    /// Истина, если `badge`/`mode` изменились с последнего `take_view` —
    /// вид бейджа/теста нужно пересчитать и записать в UI (И-Р13, ТЗ-61,
    /// ТЗ-62).
    dirty: bool,
}

impl BadgeViewState {
    /// `dirty: true` с самого начала: первое чтение должно записать в UI
    /// бейдж «Нет потока», иначе Slint-дефолт `BadgeKind.none` покажет
    /// старые блоки совместимости (ТЗ-52, ТЗ-56).
    pub(super) fn new() -> Self {
        Self { badge: BadgeState::NoStream, mode: ModeKind::default(), dirty: true }
    }

    /// Принять новый бейдж тракта и взвести пересчёт вида (И-Р13, ТЗ-52,
    /// ТЗ-60).
    pub(super) fn set_badge(&mut self, badge: BadgeState) {
        self.badge = badge;
        self.dirty = true;
    }

    /// Принять смену активного режима и взвести пересчёт вида — доступность
    /// «Теста» зависит от режима (ТЗ-62).
    pub(super) fn set_mode(&mut self, mode: ModeKind) {
        if mode != self.mode {
            self.mode = mode;
            self.dirty = true;
        }
    }

    /// Снять вид бейджа тракта и кнопки «Тест», если он устарел с прошлого
    /// чтения, — одноразовое чтение для вызывающей стороны (И-Р13, ТЗ-52,
    /// ТЗ-61, ТЗ-62). Без запущенного раннера «Теста» бейдж `test` всегда
    /// `None` — передаётся `test_view` явно.
    pub(super) fn take_view(&mut self) -> Option<(BadgeView, TestView)> {
        if self.dirty {
            self.dirty = false;
            Some((badge_view(&self.badge), test_view(&self.badge, self.mode, None)))
        } else {
            None
        }
    }
}

impl Default for BadgeViewState {
    fn default() -> Self {
        Self::new()
    }
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

    /// И-Р13: свежее состояние взводит вид бейджа один раз («Нет потока» до
    /// первого принятого бейджа), повторное чтение без изменений — `None`.
    #[test]
    fn badge_view_written_once_until_change() {
        let mut s = BadgeViewState::new();

        let (badge, _test) = s.take_view().expect("первое чтение взведено");
        assert_eq!(badge.kind, BadgeKind::NoStream);

        assert!(s.take_view().is_none());
    }

    /// ТЗ-62: доступность «Теста» зависит от режима — смена активного
    /// режима снова взводит вид бейджа/теста даже без нового бейджа.
    #[test]
    fn badge_view_redirtied_on_mode_change() {
        let mut s = BadgeViewState::new();
        assert!(s.take_view().is_some());
        assert!(s.take_view().is_none());

        s.set_mode(ModeKind::Strict);
        assert!(s.take_view().is_some());
        assert!(s.take_view().is_none());
    }
}
