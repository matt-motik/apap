//! «Тест» тракта: ключ конфигурации и сохраняемый результат (AM1.0 §2.7,
//! ADR-06, ADR-19). Здесь — минимум, нужный вердикту бейджа (§6.22); прогон
//! теста, `TestBadge` и доступность добавляются следующими шагами этапа.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::time::SystemTime;

use crate::audio::format::{OutputSampleFormat, SampleRate};
use crate::audio::path::signal::SignalPath;
use crate::settings::playback::ModeKind;

/// Ключ действующей конфигурации (термин ТЗ §2, ОВ-18). Все поля обязательны.
///
/// Поле `device` — та же временная замена `HwDeviceId` (§2.3), что и в
/// `crate::settings::playback::OptimalOpts` (см. её doc-комментарий): тип
/// в кодовой базе ещё не определён; заменить на `HwDeviceId`, когда он появится.
///
/// Решение шага AM1.0-8.5: спецификация задаёт также `Hash` и
/// `serde::Serialize/Deserialize`. У `ModeKind` и `OutputSampleFormat` этих
/// derive пока нет, поэтому они появятся вместе с хранением результатов
/// теста (ADR-19); до тех пор — `Clone, PartialEq, Eq, Debug`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConfigKey {
    pub device: String,
    pub mode: ModeKind,
    pub rate: SampleRate,
    pub format: OutputSampleFormat,
    pub channels: u8,
    pub player_version: String,
    /// `uname -r` на Linux; на других ОС — версия ОС.
    pub kernel: String,
}

impl ConfigKey {
    /// Совпадает ли ключ с конфигурацией снимка (§6.22: `rec.key == key(path)`).
    ///
    /// Сравниваются поля, выводимые из снимка: имя устройства
    /// (`output.device_name`), режим и запрошенный формат `output.requested`
    /// (частота, формат сэмпла, число каналов). `player_version` и `kernel`
    /// из снимка не выводятся. Решение шага AM1.0-8.5: вызывающий (движок)
    /// выбирает `TestRecord` из хранилища по полному `ConfigKey`, а вердикт
    /// дополнительно проверяет совпадение записи с путём.
    pub fn matches_path(&self, path: &SignalPath) -> bool {
        let req = &path.output.requested;
        self.device == path.output.device_name
            && self.mode == path.mode
            && self.rate == req.rate
            && self.format == req.sample
            && self.channels == req.channels.get()
    }
}

/// Сохраняемый результат (ADR-19). Только Passed/Failed; Incomplete не сохраняется (ТЗ-67).
#[derive(Clone, PartialEq, Debug)]
pub struct TestRecord {
    pub key: ConfigKey,
    pub passed: bool,
    pub date: SystemTime,
}
