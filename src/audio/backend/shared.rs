//! Shared-бэкенд вывода (§2.3, этап С3). Реализовано только подмножество
//! `enumerate`: `caps`/`start` появятся вместе со `StreamFormat`/
//! `SharedRender`/`StartError` на следующих этапах.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use super::{BackendError, SharedDeviceId, SharedDeviceInfo};
use crate::audio::output::{classify_device, AudioHost, CpalHost, DeviceInfo};

/// Перечисление Shared-устройств (§2.3). `caps`/`start` — подмножество
/// полного трейта из спецификации появится на последующих этапах.
pub trait SharedBackend: Send {
    fn enumerate(&mut self) -> Result<Vec<SharedDeviceInfo>, BackendError>;
}

/// Shared-бэкенд поверх `AudioHost` (cpal). На Linux спецификация требует
/// показывать в Shared только прокси сервера (default/PipeWire/Pulse) —
/// фильтрация по категории добавится вместе с разделением Shared/Exclusive
/// (ADR-07); пока, как и легаси-UI, список не фильтруется (мост С3).
pub struct CpalSharedBackend<H: AudioHost + Send = CpalHost> {
    host: H,
}

impl CpalSharedBackend<CpalHost> {
    pub fn new() -> CpalSharedBackend<CpalHost> {
        CpalSharedBackend { host: CpalHost }
    }
}

impl Default for CpalSharedBackend<CpalHost> {
    fn default() -> Self {
        CpalSharedBackend::new()
    }
}

impl<H: AudioHost + Send> CpalSharedBackend<H> {
    pub fn with_host(host: H) -> CpalSharedBackend<H> {
        CpalSharedBackend { host }
    }
}

impl<H: AudioHost + Send> SharedBackend for CpalSharedBackend<H> {
    fn enumerate(&mut self) -> Result<Vec<SharedDeviceInfo>, BackendError> {
        let devices = self.host.devices();
        let default_name = self.host.default_name();
        Ok(devices
            .into_iter()
            .map(|dev| {
                let is_default = default_name.as_deref() == Some(dev.name.as_str());
                SharedDeviceInfo {
                    id: SharedDeviceId::new(dev.id.clone()),
                    name: dev.name.clone(),
                    is_default,
                    legacy: dev,
                }
            })
            .collect())
    }
}

/// Фейк для тестов: отдаёт заранее заданный список устройств или ошибку и
/// считает число вызовов `enumerate` (§2.3).
pub struct FakeSharedBackend {
    result: Result<Vec<SharedDeviceInfo>, BackendError>,
    enumerations: usize,
}

impl FakeSharedBackend {
    pub fn with_devices(devices: Vec<SharedDeviceInfo>) -> FakeSharedBackend {
        FakeSharedBackend { result: Ok(devices), enumerations: 0 }
    }

    pub fn with_error(err: BackendError) -> FakeSharedBackend {
        FakeSharedBackend { result: Err(err), enumerations: 0 }
    }

    /// Сколько раз был вызван `enumerate`.
    pub fn enumerations(&self) -> usize {
        self.enumerations
    }
}

impl SharedBackend for FakeSharedBackend {
    fn enumerate(&mut self) -> Result<Vec<SharedDeviceInfo>, BackendError> {
        self.enumerations += 1;
        self.result.clone()
    }
}

/// Готовый `SharedDeviceInfo` с нейтральными легаси-полями для тестов без
/// реального cpal-хоста (мост С3 — `DeviceInfo` нужен только для совместимости
/// со списком устройств в UI).
pub fn fake_device(id: &str, name: &str) -> SharedDeviceInfo {
    let category = classify_device(id, name);
    let legacy = DeviceInfo {
        id: id.to_string(),
        name: name.to_string(),
        channels: 2,
        sample_rate: 44_100,
        sample_format: cpal::SampleFormat::F32,
        buffer_size: cpal::SupportedBufferSize::Range { min: 64, max: 4096 },
        supported: Vec::new(),
        category,
        supported_rates: vec![44_100],
        supported_formats: vec![cpal::SampleFormat::F32],
        exclusive_capable: false,
    };
    SharedDeviceInfo { id: SharedDeviceId::new(id), name: name.to_string(), is_default: false, legacy }
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<CpalSharedBackend<CpalHost>>();
    assert_send::<FakeSharedBackend>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    struct MockHost {
        devices: Vec<DeviceInfo>,
        default_name: Option<String>,
    }

    impl AudioHost for MockHost {
        fn devices(&self) -> Vec<DeviceInfo> {
            self.devices.clone()
        }

        fn default_name(&self) -> Option<String> {
            self.default_name.clone()
        }
    }

    fn mock_device(id: &str, name: &str) -> DeviceInfo {
        fake_device(id, name).legacy
    }

    #[test]
    fn cpal_shared_backend_marks_default() {
        let host = MockHost {
            devices: vec![mock_device("hw:0", "Speakers"), mock_device("hw:1", "HDMI")],
            default_name: Some("HDMI".to_string()),
        };
        let mut backend = CpalSharedBackend::with_host(host);

        let devices = backend.enumerate().expect("enumerate ok");

        assert_eq!(devices.len(), 2);
        assert!(!devices[0].is_default);
        assert_eq!(devices[0].name, "Speakers");
        assert!(devices[1].is_default);
        assert_eq!(devices[1].name, "HDMI");
    }

    #[test]
    fn fake_shared_backend_counts_and_fails() {
        let mut ok_backend = FakeSharedBackend::with_devices(vec![fake_device("hw:0", "Speakers")]);
        assert_eq!(ok_backend.enumerate().expect("ok 1").len(), 1);
        assert_eq!(ok_backend.enumerate().expect("ok 2").len(), 1);
        assert_eq!(ok_backend.enumerations(), 2);

        let mut err_backend = FakeSharedBackend::with_error(BackendError::Unavailable("no host".to_string()));
        match err_backend.enumerate() {
            Err(BackendError::Unavailable(msg)) => assert_eq!(msg, "no host"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert_eq!(err_backend.enumerations(), 1);
    }
}
