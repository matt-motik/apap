//! Кэш перечисления устройств и наблюдатель подключения/отключения (§2.3,
//! ADR-16, ТЗ-105, ТЗ-114). Движок перечисляет устройства один раз по
//! событию `DeviceWatcher` или по команде «Обновить» и обновляет каталог.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use super::shared::SharedBackend;
use super::{BackendError, SharedDeviceId, SharedDeviceInfo};
use std::sync::{Arc, Mutex};

/// Кэш перечисления Shared-устройств (ADR-16). Меняется только по событию
/// `DeviceWatcher` или по команде «Обновить» — не на каждое открытие трека.
///
/// В спецификации (§2.3) у `DeviceCatalog` есть ещё поле `hw: Vec<HwDeviceInfo>`
/// для `ExclusiveBackend`; на этапе С3 Exclusive-режим всё ещё идёт через
/// легаси-путь cpal (мост), поэтому поле `hw` здесь не заводится — заглушка
/// без реального производителя данных противоречила бы запрету на заглушки.
#[derive(Clone, Debug, Default)]
pub struct DeviceCatalog {
    pub shared: Vec<SharedDeviceInfo>,
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

    pub fn find(&self, id: &SharedDeviceId) -> Option<&SharedDeviceInfo> {
        self.shared.iter().find(|dev| &dev.id == id)
    }

    pub fn default_device(&self) -> Option<&SharedDeviceInfo> {
        self.shared.iter().find(|dev| dev.is_default)
    }
}

fn devices_match(a: &[SharedDeviceInfo], b: &[SharedDeviceInfo]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.id == y.id && x.name == y.name && x.is_default == y.is_default)
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
}
