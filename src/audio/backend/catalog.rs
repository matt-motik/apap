//! Кэш перечисления устройств и наблюдатель подключения/отключения (§2.3,
//! ADR-16, ТЗ-105, ТЗ-114). Движок перечисляет устройства один раз по
//! событию `DeviceWatcher` или по команде «Обновить» и обновляет каталог.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use super::exclusive::ExclusiveBackend;
use super::shared::SharedBackend;
use super::{BackendError, HwDeviceId, SharedDeviceId, SharedDeviceInfo};
use crate::audio::reservation::SinkCard;
use std::sync::{Arc, Mutex};

/// Устройство ALSA Exclusive-перечисления на момент обновления каталога
/// (§2.3, ADR-24, ТЗ-143): стабильный `HwDeviceId`, номера карты и устройства
/// ALSA на момент перечисления и человекочитаемое имя.
#[derive(Clone, Debug)]
pub struct HwDeviceInfo {
    pub id: HwDeviceId,
    pub card: u32,
    pub device: u32,
    pub name: String,
}

/// Кэш перечисления устройств (ADR-16, §2.3): `shared` — Shared-перечисление,
/// `hw` — ALSA Exclusive-перечисление (ADR-24, ТЗ-143). Меняется только по
/// событию `DeviceWatcher` или по команде «Обновить» — не на каждое открытие
/// трека.
///
/// На этапе С3 Exclusive-режим всё ещё идёт через легаси-путь cpal (мост),
/// поэтому `hw` пока не заполняется реальным перечислителем и остаётся
/// пустым по умолчанию; производитель данных появится вместе с
/// `AlsaExclusiveBackend::enumerate` (ТЗ-143).
#[derive(Clone, Debug, Default)]
pub struct DeviceCatalog {
    pub shared: Vec<SharedDeviceInfo>,
    pub hw: Vec<HwDeviceInfo>,
    pub generation: u64,
}

impl DeviceCatalog {
    /// Перечисляет устройства один раз через `backend` и, если список
    /// изменился, заменяет `shared` и увеличивает `generation` (ADR-16).
    /// Сравнение — по последовательности `(id, name, is_default)`, так как
    /// легаси `DeviceInfo` внутри `SharedDeviceInfo` не реализует `PartialEq`.
    /// При ошибке каталог не изменяется.
    pub fn refresh(&mut self, backend: &mut dyn SharedBackend) -> Result<bool, BackendError> {
        let devices = backend.enumerate()?;
        if devices_match(&self.shared, &devices) {
            return Ok(false);
        }
        self.shared = devices;
        self.generation = self.generation.wrapping_add(1);
        Ok(true)
    }

    /// Перечисляет аппаратные устройства Exclusive-бэкенда один раз и, если
    /// список изменился, заменяет `hw` и увеличивает `generation` (§2.3,
    /// ADR-24, ТЗ-42). Сравнение — по последовательности `(id, card, device,
    /// name)`, так как `HwDeviceInfo` не реализует `PartialEq`. При ошибке
    /// каталог не изменяется — симметрично `refresh`.
    pub fn refresh_hw(&mut self, backend: &mut dyn ExclusiveBackend) -> Result<bool, BackendError> {
        let devices = backend.enumerate()?;
        if hw_devices_match(&self.hw, &devices) {
            return Ok(false);
        }
        self.hw = devices;
        self.generation = self.generation.wrapping_add(1);
        Ok(true)
    }

    pub fn find(&self, id: &SharedDeviceId) -> Option<&SharedDeviceInfo> {
        self.shared.iter().find(|dev| &dev.id == id)
    }

    pub fn default_device(&self) -> Option<&SharedDeviceInfo> {
        self.shared.iter().find(|dev| dev.is_default)
    }

    /// Аппаратное устройство карты системного выхода (§2.3, ADR-24, ТЗ-143):
    /// среди записей `hw` с номером карты `sink.card` — запись с тем же
    /// `device`, если `sink.device` задан и найдена; иначе первая запись
    /// этой карты в порядке каталога; нет записей этой карты — `None`.
    pub fn hw_for_sink(&self, sink: SinkCard) -> Option<&HwDeviceId> {
        if let Some(device) = sink.device {
            if let Some(dev) = self.hw.iter().find(|dev| dev.card == sink.card && dev.device == device) {
                return Some(&dev.id);
            }
        }
        self.hw.iter().find(|dev| dev.card == sink.card).map(|dev| &dev.id)
    }

    /// Находит запись каталога по `HwDeviceId`, допуская устаревший
    /// численный формат (AM1.0-B16, ADR-24, ТЗ-126): старый `settings.toml`
    /// мог сохранить `hw:CARD=<N>,DEV=<D>`, где `N` — номер карты cpal на
    /// момент сохранения, а не стабильный строковый id (`hw:CARD=<id>,
    /// DEV=<D>`, который строит `build_hw_id`). Сначала ищется точное
    /// совпадение id; если его нет и строка разбирается по численному
    /// шаблону — ищется запись с тем же `card`/`device`. Иначе — `None`.
    pub fn canonical_hw(&self, id: &HwDeviceId) -> Option<&HwDeviceInfo> {
        if let Some(found) = self.hw.iter().find(|dev| &dev.id == id) {
            return Some(found);
        }
        let (card, device) = parse_numeric_hw_id(id.as_str())?;
        self.hw.iter().find(|dev| dev.card == card && dev.device == device)
    }
}

/// Разбирает устаревший численный `HwDeviceId` вида `hw:CARD=<N>,DEV=<D>`
/// (легаси-формат cpal, AM1.0-B16, ТЗ-126) в номера карты и устройства.
/// `None`, если строка не соответствует этому шаблону — в частности, для
/// канонической формы с нечисловым id карты (`hw:CARD=PCH,DEV=0`).
fn parse_numeric_hw_id(id: &str) -> Option<(u32, u32)> {
    let rest = id.strip_prefix("hw:CARD=")?;
    let (card, device) = rest.split_once(",DEV=")?;
    let card = card.parse::<u32>().ok()?;
    let device = device.parse::<u32>().ok()?;
    Some((card, device))
}

fn devices_match(a: &[SharedDeviceInfo], b: &[SharedDeviceInfo]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.id == y.id && x.name == y.name && x.is_default == y.is_default)
}

fn hw_devices_match(a: &[HwDeviceInfo], b: &[HwDeviceInfo]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.id == y.id && x.card == y.card && x.device == y.device && x.name == y.name)
}

/// Наблюдатель подключения/отключения устройств (ADR-16, ТЗ-105): на Linux —
/// поток `apap-devwatch` поверх `/proc/asound/cards`, вызывает `notify` при
/// изменении списка. Боевая реализация появится вместе с ТЗ-126/ТЗ-127.
pub trait DeviceWatcher: Send {
    fn start(&mut self, notify: Box<dyn Fn() + Send>);
}

type NotifySlot = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

/// Тестовая реализация `DeviceWatcher`: события генерируются вручную через
/// `FakeWatchTrigger` вместо реального опроса `/proc/asound/cards` (ADR-16,
/// ТЗ-105, ТЗ-114).
#[derive(Default)]
pub struct FakeDeviceWatcher {
    slot: NotifySlot,
}

impl FakeDeviceWatcher {
    pub fn new() -> FakeDeviceWatcher {
        FakeDeviceWatcher::default()
    }

    /// Возвращает управляющий хэндл: `start` обычно перемещает `self` в
    /// движок, а тест продолжает генерировать события через хэндл.
    pub fn trigger_handle(&self) -> FakeWatchTrigger {
        FakeWatchTrigger { slot: Arc::clone(&self.slot) }
    }
}

impl DeviceWatcher for FakeDeviceWatcher {
    fn start(&mut self, notify: Box<dyn Fn() + Send>) {
        let mut guard = self.slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(notify);
    }
}

/// Клонируемый хэндл для ручной генерации событий `FakeDeviceWatcher` в
/// тестах (ADR-16, ТЗ-105, ТЗ-114).
#[derive(Clone, Default)]
pub struct FakeWatchTrigger {
    slot: NotifySlot,
}

impl FakeWatchTrigger {
    /// Был ли уже вызван `DeviceWatcher::start`.
    pub fn started(&self) -> bool {
        let guard = self.slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.is_some()
    }

    /// Вызывает сохранённый колбэк `notify`. Возвращает `false`, если
    /// наблюдатель ещё не запущен.
    pub fn fire(&self) -> bool {
        let guard = self.slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(notify) => {
                notify();
                true
            }
            None => false,
        }
    }
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send::<FakeDeviceWatcher>();
    assert_send_sync::<FakeWatchTrigger>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;
    use crate::audio::backend::shared::{fake_device, FakeSharedBackend};
    use crate::audio::testing::FakeExclusiveBackend;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn catalog_refresh_bumps_generation_only_on_change() {
        let mut catalog = DeviceCatalog::default();
        let mut backend =
            FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers"), fake_device("hw:1", "HDMI")]);

        assert!(catalog.refresh(&mut backend).expect("refresh 1 ok"));
        assert_eq!(catalog.generation, 1);
        assert_eq!(catalog.shared.len(), 2);

        let mut same_backend =
            FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers"), fake_device("hw:1", "HDMI")]);
        assert!(!catalog.refresh(&mut same_backend).expect("refresh 2 ok"));
        assert_eq!(catalog.generation, 1);

        let mut changed_backend = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        assert!(catalog.refresh(&mut changed_backend).expect("refresh 3 ok"));
        assert_eq!(catalog.generation, 2);
        assert_eq!(catalog.shared.len(), 1);
    }

    #[test]
    fn catalog_refresh_error_keeps_state() {
        let mut catalog = DeviceCatalog::default();
        let mut backend = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        assert!(catalog.refresh(&mut backend).expect("refresh ok"));
        let generation = catalog.generation;
        let shared = catalog.shared.clone();

        let mut err_backend = FakeSharedBackend::with_error(BackendError::Unavailable("no host".to_string()));
        assert!(catalog.refresh(&mut err_backend).is_err());
        assert_eq!(catalog.generation, generation);
        assert_eq!(catalog.shared.len(), shared.len());
    }

    #[test]
    fn catalog_default_and_find() {
        let mut catalog = DeviceCatalog::default();
        assert!(catalog.default_device().is_none());
        assert!(catalog.find(&SharedDeviceId::new("hw:0")).is_none());

        let mut dev0 = fake_device("hw:0", "Speakers");
        dev0.is_default = false;
        let mut dev1 = fake_device("hw:1", "HDMI");
        dev1.is_default = true;
        let mut backend = FakeSharedBackend::with_devices(vec![dev0, dev1]);
        catalog.refresh(&mut backend).expect("refresh ok");

        assert_eq!(catalog.find(&SharedDeviceId::new("hw:1")).expect("found").name, "HDMI");
        assert!(catalog.find(&SharedDeviceId::new("hw:2")).is_none());
        assert_eq!(catalog.default_device().expect("default").name, "HDMI");
    }

    #[test]
    fn fake_watcher_fires_after_start() {
        let mut watcher = FakeDeviceWatcher::new();
        let trigger = watcher.trigger_handle();
        assert!(!trigger.started());
        assert!(!trigger.fire());

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_cb = Arc::clone(&counter);
        watcher.start(Box::new(move || {
            counter_cb.fetch_add(1, Ordering::SeqCst);
        }));

        assert!(trigger.started());
        assert!(trigger.fire());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(trigger.fire());
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    fn fake_hw(id: &str, card: u32, device: u32, name: &str) -> HwDeviceInfo {
        let id: HwDeviceId = id.parse().expect("hw id");
        HwDeviceInfo { id, card, device, name: name.to_string() }
    }

    #[test]
    fn hw_for_sink_exact_device_match() {
        let catalog = DeviceCatalog {
            hw: vec![
                fake_hw("hw:CARD=PCH,DEV=0", 0, 0, "PCH 0"),
                fake_hw("hw:CARD=PCH,DEV=1", 0, 1, "PCH 1"),
            ],
            ..Default::default()
        };

        let found = catalog.hw_for_sink(SinkCard { card: 0, device: Some(1) }).expect("found");
        assert_eq!(found.as_str(), "hw:CARD=PCH,DEV=1");
    }

    #[test]
    fn hw_for_sink_device_absent_picks_first_of_card() {
        let catalog = DeviceCatalog {
            hw: vec![fake_hw("hw:CARD=PCH,DEV=0", 0, 0, "PCH 0"), fake_hw("hw:CARD=USB,DEV=0", 1, 0, "USB 0")],
            ..Default::default()
        };

        let found = catalog.hw_for_sink(SinkCard { card: 0, device: None }).expect("found");
        assert_eq!(found.as_str(), "hw:CARD=PCH,DEV=0");
    }

    #[test]
    fn hw_for_sink_device_not_on_card_picks_first_of_card() {
        let catalog = DeviceCatalog {
            hw: vec![
                fake_hw("hw:CARD=PCH,DEV=0", 0, 0, "PCH 0"),
                fake_hw("hw:CARD=PCH,DEV=1", 0, 1, "PCH 1"),
            ],
            ..Default::default()
        };

        let found = catalog.hw_for_sink(SinkCard { card: 0, device: Some(9) }).expect("found");
        assert_eq!(found.as_str(), "hw:CARD=PCH,DEV=0");
    }

    #[test]
    fn hw_for_sink_unknown_card_is_none() {
        let catalog = DeviceCatalog { hw: vec![fake_hw("hw:CARD=PCH,DEV=0", 0, 0, "PCH 0")], ..Default::default() };

        assert!(catalog.hw_for_sink(SinkCard { card: 5, device: None }).is_none());
    }

    #[test]
    fn refresh_hw_bumps_generation_only_on_change() {
        let mut catalog = DeviceCatalog::default();
        let mut backend = FakeExclusiveBackend::new()
            .with_enumerate(Ok(vec![fake_hw("hw:CARD=PCH,DEV=0", 0, 0, "PCH 0"), fake_hw("hw:CARD=USB,DEV=0", 1, 0, "USB 0")]));

        assert!(catalog.refresh_hw(&mut backend).expect("refresh 1 ok"));
        assert_eq!(catalog.generation, 1);
        assert_eq!(catalog.hw.len(), 2);

        let mut same_backend = FakeExclusiveBackend::new().with_enumerate(Ok(vec![
            fake_hw("hw:CARD=PCH,DEV=0", 0, 0, "PCH 0"),
            fake_hw("hw:CARD=USB,DEV=0", 1, 0, "USB 0"),
        ]));
        assert!(!catalog.refresh_hw(&mut same_backend).expect("refresh 2 ok"));
        assert_eq!(catalog.generation, 1);

        let mut changed_backend =
            FakeExclusiveBackend::new().with_enumerate(Ok(vec![fake_hw("hw:CARD=PCH,DEV=0", 0, 0, "PCH 0")]));
        assert!(catalog.refresh_hw(&mut changed_backend).expect("refresh 3 ok"));
        assert_eq!(catalog.generation, 2);
        assert_eq!(catalog.hw.len(), 1);
    }

    #[test]
    fn canonical_hw_exact_match() {
        let catalog = DeviceCatalog { hw: vec![fake_hw("hw:CARD=DX,DEV=0", 0, 0, "PCH 0")], ..Default::default() };

        let found = catalog.canonical_hw(&"hw:CARD=DX,DEV=0".parse().expect("hw id")).expect("found");
        assert_eq!(found.id.as_str(), "hw:CARD=DX,DEV=0");
    }

    #[test]
    fn canonical_hw_numeric_legacy_maps_to_canonical() {
        let catalog = DeviceCatalog { hw: vec![fake_hw("hw:CARD=DX,DEV=0", 0, 0, "PCH 0")], ..Default::default() };

        let found = catalog.canonical_hw(&"hw:CARD=0,DEV=0".parse().expect("hw id")).expect("found");
        assert_eq!(found.id.as_str(), "hw:CARD=DX,DEV=0");
        assert_eq!(found.card, 0);
        assert_eq!(found.device, 0);
    }

    #[test]
    fn canonical_hw_numeric_unknown_card_is_none() {
        let catalog = DeviceCatalog { hw: vec![fake_hw("hw:CARD=DX,DEV=0", 0, 0, "PCH 0")], ..Default::default() };

        assert!(catalog.canonical_hw(&"hw:CARD=5,DEV=0".parse().expect("hw id")).is_none());
    }

    #[test]
    fn canonical_hw_malformed_id_is_none() {
        let catalog = DeviceCatalog { hw: vec![fake_hw("hw:CARD=DX,DEV=0", 0, 0, "PCH 0")], ..Default::default() };

        assert!(catalog.canonical_hw(&"hw:CARD=PCH,DEV=0".parse().expect("hw id")).is_none());
    }

    #[test]
    fn refresh_hw_error_keeps_state() {
        let mut catalog = DeviceCatalog::default();
        let mut backend =
            FakeExclusiveBackend::new().with_enumerate(Ok(vec![fake_hw("hw:CARD=PCH,DEV=0", 0, 0, "PCH 0")]));
        assert!(catalog.refresh_hw(&mut backend).expect("refresh ok"));
        let generation = catalog.generation;
        let hw_len = catalog.hw.len();

        let mut err_backend =
            FakeExclusiveBackend::new().with_enumerate(Err(BackendError::Unavailable("no host".to_string())));
        assert!(catalog.refresh_hw(&mut err_backend).is_err());
        assert_eq!(catalog.generation, generation);
        assert_eq!(catalog.hw.len(), hw_len);
    }
}
