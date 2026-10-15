//! «Тест» тракта: ключ конфигурации, сохраняемый результат, кнопка-бейдж
//! «Тест» и её доступность (AM1.0 §2.7, §6.22, ADR-06, ADR-19, ТЗ-61, ТЗ-62).
//! Прогон теста (§6.23: `TestOutcome`, `TestRun`, сверка зеркала) и хранение
//! результатов (`bp_tests.toml`) сюда не входят — они относятся к этапу «Тест».
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::time::{Duration, SystemTime};

use crate::audio::format::{OutputSampleFormat, SampleRate};
use crate::audio::path::signal::SignalPath;
use crate::audio::verdict::{BadgeState, ExclusiveBadge, NbpReason, NbpSummary};
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

/// Причина незавершённого теста (§2.7, §6.23, ТЗ-67). Нужна `TestBadge::Unknown`
/// для подсказки «?»; сам прогон, порождающий эти причины, — этап «Тест».
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IncompleteWhy {
    Underrun,
    Xrun,
    MirrorOverflow,
    Seek,
    TrackChanged,
    ModeChanged,
    DeviceChanged,
    LockOpened,
}

/// Кнопка-бейдж «Тест» (ТЗ-61).
#[derive(Clone, PartialEq, Debug)]
pub enum TestBadge {
    /// ✓ — есть успешный тест для действующей конфигурации.
    Passed { date: SystemTime },
    /// ✕ — тест не пройден.
    Failed { date: SystemTime },
    /// Нет результата для конфигурации или последний тест не завершён.
    Unknown(Option<IncompleteWhy>),
    /// Идёт тест: прошло `elapsed` из не более `max` (ТЗ-65, ОВ-17).
    Running { elapsed: Duration, max: Duration },
}

/// Доступность кнопки «Тест» (§2.7, §6.22, ТЗ-61, ТЗ-62, ОВ-20).
#[derive(Clone, PartialEq, Debug)]
pub enum TestAvailability {
    Enabled,
    Disabled(TestBlocker),
}

/// Что мешает «Тесту»; текст подсказки строит UI (§2.7, ТЗ-61).
#[derive(Clone, PartialEq, Debug)]
pub enum TestBlocker {
    /// «Проверка недоступна в Shared» (ТЗ-62).
    SharedMode,
    /// «Запустите воспроизведение» (ТЗ-65).
    NoStream,
    /// Преобразование с потерями: подсказка + ссылка на «Путь сигнала» (ОВ-20).
    Lossy(NbpReason),
    /// Параметры драйвера не подтверждены (ОВ-14).
    DriverParamsUnconfirmed,
    /// Включён mute: бейдж «Mute», на выходе тишина (ОВС-18, ТЗ-139).
    Muted,
}

/// Доступность «Теста» — чистая функция бейджа и режима (§6.22, ТЗ-61, ТЗ-62,
/// ОВ-14, ОВ-20, ОВС-18).
///
/// Активна при `BitPerfect` и при единственной причине `TestFailed`
/// (повторный тест, ОВ-20); иначе неактивна с блокером.
///
/// Решение шага AM1.0-8.5 (пробел §6.22): бейдж `Shared(_)` вне Совместимого
/// режима (Exclusive не получен) тоже даёт `SharedMode` — тест сверяет точку
/// передачи монопольного вывода и в Shared невозможен (ТЗ-62). При
/// `primary == TestFailed` и `more > 0` блокер — `Lossy(TestFailed)` по
/// правилу «иначе» §6.22: сводка не хранит вторую причину.
pub fn test_availability(badge: &BadgeState, mode: ModeKind) -> TestAvailability {
    if mode == ModeKind::Compatible {
        return TestAvailability::Disabled(TestBlocker::SharedMode);
    }
    let summary = match badge {
        BadgeState::NoStream => return TestAvailability::Disabled(TestBlocker::NoStream),
        BadgeState::Shared(_) => return TestAvailability::Disabled(TestBlocker::SharedMode),
        BadgeState::Exclusive(ExclusiveBadge::Muted) => return TestAvailability::Disabled(TestBlocker::Muted),
        BadgeState::Exclusive(ExclusiveBadge::BitPerfect(_)) => return TestAvailability::Enabled,
        BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(s)) => s,
    };
    match summary {
        NbpSummary { primary: NbpReason::TestFailed, more: 0 } => TestAvailability::Enabled,
        NbpSummary { primary: NbpReason::DriverParamsUnconfirmed, .. } => {
            TestAvailability::Disabled(TestBlocker::DriverParamsUnconfirmed)
        }
        NbpSummary { primary, .. } => TestAvailability::Disabled(TestBlocker::Lossy(*primary)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use std::num::NonZeroU8;

    use super::*;
    use crate::audio::backend::{AccessKind, DriverFormat, OutputApi};
    use crate::audio::format::{BitDepth, ChannelLayout, Codec, Container, SourceFormat, SourceKind, StreamFormat};
    use crate::audio::path::transform::{Cause, Confidence, Transform, TransformKind};
    use crate::audio::path::{OpenedFacts, OsStage, PathPlan, ReservationView, Route};
    use crate::audio::session::RingPayload;
    use crate::audio::verdict::{verdict, GainState, SharedBadge};
    use crate::settings::playback::Volume;

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn fmt(hz: u32) -> StreamFormat {
        StreamFormat { rate: sr(hz), sample: OutputSampleFormat::S32Le, channels: NonZeroU8::new(2).unwrap() }
    }

    /// Снимок Оптимального режима с преобразованиями `transforms` (§3.1).
    fn path(access: AccessKind, driver: DriverFormat, transforms: &[TransformKind]) -> SignalPath {
        let plan = PathPlan {
            mode: ModeKind::Optimal,
            source: SourceFormat {
                container: Container::Flac,
                codec: Codec::Flac,
                lossy: false,
                rate: sr(44_100),
                layout: ChannelLayout::stereo(),
                kind: SourceKind::Pcm { bits: BitDepth::new(16).unwrap() },
            },
            route: Route::Pcm,
            transforms: transforms
                .iter()
                .cloned()
                .map(|kind| Transform { kind, cause: Cause::ModeRule, confidence: Confidence::Assumed })
                .collect(),
            target: fmt(44_100),
            ring: RingPayload::ExactI32 { valid_bits: BitDepth::new(16).unwrap() },
            dsd_chain: None,
        };
        let facts = OpenedFacts {
            access,
            api: OutputApi::AlsaHw,
            device_name: "hw:0".to_owned(),
            driver,
            reservation: ReservationView::Held,
            os: OsStage::NotInvolved,
            device_buffer: None,
        };
        SignalPath::from_opened(plan, facts, 1)
    }

    fn exclusive(transforms: &[TransformKind]) -> SignalPath {
        path(AccessKind::Exclusive, DriverFormat::Confirmed(fmt(44_100)), transforms)
    }

    fn failed_record() -> TestRecord {
        let key = ConfigKey {
            device: "hw:0".to_owned(),
            mode: ModeKind::Optimal,
            rate: sr(44_100),
            format: OutputSampleFormat::S32Le,
            channels: 2,
            player_version: "0.1.0".to_owned(),
            kernel: "6.8.0".to_owned(),
        };
        TestRecord { key, passed: false, date: SystemTime::UNIX_EPOCH }
    }

    fn nbp(primary: NbpReason, more: u8) -> BadgeState {
        BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(NbpSummary { primary, more }))
    }

    fn disabled(b: TestBlocker) -> TestAvailability {
        TestAvailability::Disabled(b)
    }

    /// §7.2, ТЗ-61, ТЗ-62, ОВ-14, ОВ-20, ОВС-18: каждый блокер и случаи «доступно».
    #[test]
    fn test_availability_rules() {
        let opt = ModeKind::Optimal;

        // BP → активна.
        let bp = verdict(&exclusive(&[]), GainState::NotApplicable, None);
        assert!(matches!(bp, BadgeState::Exclusive(ExclusiveBadge::BitPerfect(_))));
        assert_eq!(test_availability(&bp, opt), TestAvailability::Enabled);
        assert_eq!(test_availability(&bp, ModeKind::Strict), TestAvailability::Enabled);

        // ✕ (единственная причина TestFailed) → активна, повторный тест (ОВ-20).
        let rec = failed_record();
        let failed = verdict(&exclusive(&[]), GainState::NotApplicable, Some(&rec));
        assert_eq!(failed, nbp(NbpReason::TestFailed, 0));
        assert_eq!(test_availability(&failed, opt), TestAvailability::Enabled);

        // Громкость 80 % → неактивна с «громкость».
        let v80 = verdict(&exclusive(&[]), GainState::Scaled(Volume::new(80).unwrap()), None);
        assert_eq!(v80, nbp(NbpReason::Volume, 0));
        assert_eq!(test_availability(&v80, opt), disabled(TestBlocker::Lossy(NbpReason::Volume)));

        // ✕ вместе с другой причиной — уже не «единственная», тест недоступен.
        let failed_vol = verdict(&exclusive(&[]), GainState::Scaled(Volume::new(80).unwrap()), Some(&rec));
        assert_eq!(failed_vol, nbp(NbpReason::TestFailed, 1));
        assert_eq!(test_availability(&failed_vol, opt), disabled(TestBlocker::Lossy(NbpReason::TestFailed)));

        // Параметры драйвера не подтверждены (ОВ-14) — и при других причинах.
        let unreadable = DriverFormat::Unreadable { requested: fmt(44_100) };
        let drv = verdict(&path(AccessKind::Exclusive, unreadable, &[]), GainState::NotApplicable, None);
        assert_eq!(drv, nbp(NbpReason::DriverParamsUnconfirmed, 0));
        assert_eq!(test_availability(&drv, opt), disabled(TestBlocker::DriverParamsUnconfirmed));
        let drv_more = nbp(NbpReason::DriverParamsUnconfirmed, 2);
        assert_eq!(test_availability(&drv_more, opt), disabled(TestBlocker::DriverParamsUnconfirmed));

        // Mute в Exclusive (ОВС-18).
        let muted = verdict(&exclusive(&[]), GainState::Muted, None);
        assert_eq!(muted, BadgeState::Exclusive(ExclusiveBadge::Muted));
        assert_eq!(test_availability(&muted, opt), disabled(TestBlocker::Muted));

        // Нет потока (ТЗ-65).
        assert_eq!(test_availability(&BadgeState::NoStream, opt), disabled(TestBlocker::NoStream));

        // Совместимый → «проверка недоступна в Shared» при любом бейдже (ТЗ-62).
        let shared = verdict(&path(AccessKind::Shared, unreadable, &[]), GainState::Unity, None);
        assert_eq!(shared, BadgeState::Shared(SharedBadge::Clean));
        for b in [&shared, &bp, &BadgeState::NoStream] {
            assert_eq!(test_availability(b, ModeKind::Compatible), disabled(TestBlocker::SharedMode));
        }
        // Shared-бейдж вне Совместимого режима (Exclusive не получен) — тоже SharedMode.
        assert_eq!(test_availability(&shared, opt), disabled(TestBlocker::SharedMode));
    }
}
