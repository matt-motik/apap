//! Единая команда громкости (ADR-14, ТЗ-37, ТЗ-39, НФ-2, §2.11).
//!
//! Модуль чистый: функции только вычисляют итог команды по снимку состояния
//! и настроек режимов; состояние изменяет вызывающий код.

use crate::settings::playback::{ModeKind, ModeSettings, PlaybackState, Volume};

// Тип определён в модуле ввода (§2.8); реэкспорт — для команды громкости
// `VolumeCmd::Step`.
pub use crate::platform::input::Clicks;

/// Команда громкости — единая для всех источников (ADR-14, ТЗ-37, §2.11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VolumeCmd {
    /// Установить абсолютное значение (ползунок).
    Set(Volume),
    /// Сдвинуть на число щелчков по `VOLUME_STEP_PCT` (колёсико, клавиши; НФ-2).
    Step(Clicks),
}

/// Источник команды громкости (ADR-14, ТЗ-37, §2.11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VolumeSource {
    Slider,
    SliderWheel,
    TrayWheel,
    Hotkey,
}

/// Причина недоступности громкости (ТЗ-39, §2.11, §2.13).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VolumeBlock {
    /// Активен Строгий режим: громкости в нём нет.
    StrictMode,
    /// Оптимальный режим с «Фиксировать громкость на 100 %».
    Locked,
}

/// Итог команды громкости (ADR-14, §2.11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VolumeOutcome {
    /// Громкость активного режима должна стать такой.
    Changed(Volume),
    /// Новое значение совпало с текущим — изменений нет.
    Unchanged,
    /// Команда отклонена до любого изменения (И-Р13).
    Blocked(VolumeBlock),
}

/// Шаг громкости за один щелчок, проценты (НФ-2).
pub const VOLUME_STEP_PCT: i32 = 2;

impl VolumeBlock {
    /// Текст подсказки недоступного регулятора громкости (ТЗ-39, §2.13).
    pub const fn tooltip(self) -> &'static str {
        match self {
            VolumeBlock::StrictMode => "Громкость недоступна в Строгом режиме",
            VolumeBlock::Locked => {
                "Громкость зафиксирована на 100 % в настройках Оптимального режима"
            }
        }
    }
}

/// Причина недоступности громкости в активном режиме; `None` — громкость
/// доступна (ТЗ-39, ADR-14, §2.11).
pub fn volume_block(state: &PlaybackState, modes: &ModeSettings) -> Option<VolumeBlock> {
    match state.active {
        ModeKind::Strict => Some(VolumeBlock::StrictMode),
        ModeKind::Optimal if modes.optimal.volume_lock => Some(VolumeBlock::Locked),
        ModeKind::Compatible | ModeKind::Optimal => None,
    }
}

/// Вычисляет итог команды громкости для активного режима (ТЗ-37, ТЗ-39,
/// НФ-2, ADR-14, §2.11). Проверка недоступности выполняется до любого
/// вычисления (И-Р13). Функция чистая: `state` не изменяется.
pub fn apply_volume(state: &PlaybackState, modes: &ModeSettings, cmd: VolumeCmd) -> VolumeOutcome {
    if let Some(block) = volume_block(state, modes) {
        return VolumeOutcome::Blocked(block);
    }
    let current = match state.active {
        ModeKind::Compatible => state.compatible.volume,
        ModeKind::Optimal => state.optimal.volume,
        // Громкости в Строгом режиме нет (ТЗ-39); сюда не доходит —
        // `volume_block` уже вернул `StrictMode`.
        ModeKind::Strict => return VolumeOutcome::Blocked(VolumeBlock::StrictMode),
    };
    let next = match cmd {
        VolumeCmd::Set(v) => v,
        VolumeCmd::Step(Clicks(k)) => {
            let pct = i32::from(current.get())
                .saturating_add(VOLUME_STEP_PCT.saturating_mul(k))
                .clamp(0, 100);
            let Some(v) = u8::try_from(pct).ok().and_then(Volume::new) else {
                return VolumeOutcome::Unchanged;
            };
            v
        }
    };
    if next == current {
        VolumeOutcome::Unchanged
    } else {
        VolumeOutcome::Changed(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::playback::ModeGain;

    /// Громкость из процентов; значение теста обязано лежать в 0..=100.
    fn vol(pct: u8) -> Volume {
        Volume::new(pct).expect("громкость теста в диапазоне 0..=100")
    }

    /// Состояние с активным режимом `active` и громкостями Совместимого и
    /// Оптимального режимов.
    fn state(active: ModeKind, compatible: u8, optimal: u8) -> PlaybackState {
        PlaybackState {
            active,
            compatible: ModeGain { volume: vol(compatible), muted: false },
            optimal: ModeGain { volume: vol(optimal), muted: false },
            strict_muted: false,
        }
    }

    /// Настройки режимов с заданной «Фиксировать громкость на 100 %».
    fn modes(volume_lock: bool) -> ModeSettings {
        let mut modes = ModeSettings::default();
        modes.optimal.volume_lock = volume_lock;
        modes
    }

    /// Итог команды в Совместимом режиме с громкостью `from`, без фиксации.
    fn compatible_from(from: u8, cmd: VolumeCmd) -> VolumeOutcome {
        apply_volume(&state(ModeKind::Compatible, from, 100), &modes(false), cmd)
    }

    /// §7.2: тексты подсказок и причина недоступности громкости по режимам;
    /// фиксация относится только к Оптимальному (ТЗ-39, ADR-14, §2.11, §2.13).
    #[test]
    fn volume_block_tooltips() {
        assert_eq!(
            VolumeBlock::StrictMode.tooltip(),
            "Громкость недоступна в Строгом режиме"
        );
        assert_eq!(
            VolumeBlock::Locked.tooltip(),
            "Громкость зафиксирована на 100 % в настройках Оптимального режима"
        );

        for lock in [false, true] {
            assert_eq!(
                volume_block(&state(ModeKind::Strict, 40, 70), &modes(lock)),
                Some(VolumeBlock::StrictMode)
            );
            assert_eq!(
                volume_block(&state(ModeKind::Compatible, 40, 70), &modes(lock)),
                None
            );
        }
        assert_eq!(
            volume_block(&state(ModeKind::Optimal, 40, 70), &modes(true)),
            Some(VolumeBlock::Locked)
        );
        assert_eq!(
            volume_block(&state(ModeKind::Optimal, 40, 70), &modes(false)),
            None
        );
    }

    /// §7.2: `Set` и `Step` меняют громкость активного режима, шаг 2 % за
    /// щелчок (ТЗ-37, НФ-2, ADR-14).
    #[test]
    fn set_and_step_change_active_mode_volume() {
        assert_eq!(
            compatible_from(40, VolumeCmd::Set(vol(46))),
            VolumeOutcome::Changed(vol(46))
        );
        assert_eq!(
            compatible_from(40, VolumeCmd::Step(Clicks(3))),
            VolumeOutcome::Changed(vol(46))
        );
        assert_eq!(
            compatible_from(40, VolumeCmd::Step(Clicks(-1))),
            VolumeOutcome::Changed(vol(38))
        );

        let optimal = state(ModeKind::Optimal, 40, 70);
        assert_eq!(
            apply_volume(&optimal, &modes(false), VolumeCmd::Step(Clicks(1))),
            VolumeOutcome::Changed(vol(72))
        );
        assert_eq!(
            apply_volume(&optimal, &modes(false), VolumeCmd::Set(vol(40))),
            VolumeOutcome::Changed(vol(40))
        );
    }

    /// §7.2: шаг ограничен диапазоном 0..=100 и не переполняется на крайних
    /// значениях щелчков (НФ-2, ТЗ-37).
    #[test]
    fn step_clamps_to_range() {
        assert_eq!(
            compatible_from(99, VolumeCmd::Step(Clicks(1))),
            VolumeOutcome::Changed(vol(100))
        );
        assert_eq!(
            compatible_from(100, VolumeCmd::Step(Clicks(1))),
            VolumeOutcome::Unchanged
        );
        assert_eq!(
            compatible_from(1, VolumeCmd::Step(Clicks(-5))),
            VolumeOutcome::Changed(vol(0))
        );
        assert_eq!(
            compatible_from(0, VolumeCmd::Step(Clicks(-1))),
            VolumeOutcome::Unchanged
        );
        assert_eq!(
            compatible_from(50, VolumeCmd::Step(Clicks(i32::MAX))),
            VolumeOutcome::Changed(vol(100))
        );
        assert_eq!(
            compatible_from(50, VolumeCmd::Step(Clicks(i32::MIN))),
            VolumeOutcome::Changed(vol(0))
        );
    }

    /// §7.2: в Строгом режиме и в Оптимальном с фиксацией команда отклоняется
    /// до любого изменения (И-Р13, ТЗ-39, ADR-14).
    #[test]
    fn blocked_before_any_change() {
        let cmds = [
            VolumeCmd::Set(vol(10)),
            VolumeCmd::Set(vol(70)),
            VolumeCmd::Step(Clicks(1)),
            VolumeCmd::Step(Clicks(-1)),
            VolumeCmd::Step(Clicks(0)),
        ];
        for cmd in cmds {
            for lock in [false, true] {
                assert_eq!(
                    apply_volume(&state(ModeKind::Strict, 40, 70), &modes(lock), cmd),
                    VolumeOutcome::Blocked(VolumeBlock::StrictMode)
                );
            }
            assert_eq!(
                apply_volume(&state(ModeKind::Optimal, 40, 70), &modes(true), cmd),
                VolumeOutcome::Blocked(VolumeBlock::Locked)
            );
        }
    }

    /// §7.2: команда, не меняющая значение, даёт `Unchanged` (ADR-14, §2.11).
    #[test]
    fn same_value_is_unchanged() {
        assert_eq!(
            compatible_from(40, VolumeCmd::Set(vol(40))),
            VolumeOutcome::Unchanged
        );
        assert_eq!(
            compatible_from(40, VolumeCmd::Step(Clicks(0))),
            VolumeOutcome::Unchanged
        );

        let optimal = state(ModeKind::Optimal, 40, 70);
        assert_eq!(
            apply_volume(&optimal, &modes(false), VolumeCmd::Set(vol(70))),
            VolumeOutcome::Unchanged
        );
        assert_eq!(
            apply_volume(&optimal, &modes(false), VolumeCmd::Step(Clicks(0))),
            VolumeOutcome::Unchanged
        );
    }
}
