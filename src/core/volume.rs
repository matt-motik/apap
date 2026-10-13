//! Единая команда громкости (ADR-14, ТЗ-37, ТЗ-39, НФ-2, §2.11).
//!
//! ОТКЛОНЕНИЕ от §2.8: `Clicks` по спецификации живёт в `platform/input`; модуль появляется на этапе С11, до него тип объявлен здесь.
//!
//! Модуль чистый: функции только вычисляют итог команды по снимку состояния
//! и настроек режимов; состояние изменяет вызывающий код.

use crate::settings::playback::{ModeKind, ModeSettings, PlaybackState, Volume};

/// Нормализованные щелчки колёсика: положительные — громче (НФ-2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Clicks(pub i32);

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
