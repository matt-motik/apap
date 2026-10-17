//! Эксклюзивный бэкенд ALSA: перечисление карт через `ctl` и возможности
//! открытого PCM (§2.3, §6.6, ADR-24, ТЗ-42…ТЗ-47, ТЗ-143). Резервирование,
//! открытие `hw:` PCM и поток вывода — отдельные этапы (`acquire`,
//! `configure`, `start` трейта `ExclusiveBackend`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use smallvec::SmallVec;

use super::catalog::HwDeviceInfo;
use super::exclusive::DeviceClaim;
use super::{AccessKind, BackendCaps, BackendError, DeviceCaps, HwDeviceId, OutputApi, RateSet};
use crate::audio::error::CaptureFailure;
use crate::audio::format::{OutputSampleFormat, SampleRate};

/// Частоты, которые проверяются первыми через `test_rate` (§6.6): если
/// устройство принимает любую из них, `DeviceCaps::rates` — дискретный
/// список, иначе — диапазон `get_rate_min`/`get_rate_max`.
const STANDARD_RATES_HZ: [u32; 10] =
    [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000];

/// Соответствие форматов ALSA форматам `OutputSampleFormat` (§2.1, ОВ-32):
/// `S243LE` — имя варианта крейта `alsa` для `SND_PCM_FORMAT_S24_3LE`.
const FORMAT_MAP: [(alsa::pcm::Format, OutputSampleFormat); 5] = [
    (alsa::pcm::Format::S16LE, OutputSampleFormat::S16Le),
    (alsa::pcm::Format::S243LE, OutputSampleFormat::S24_3Le),
    (alsa::pcm::Format::S24LE, OutputSampleFormat::S24Le),
    (alsa::pcm::Format::S32LE, OutputSampleFormat::S32Le),
    (alsa::pcm::Format::FloatLE, OutputSampleFormat::F32),
];

/// Эксклюзивный бэкенд ALSA (§2.3, ADR-24). Перечисление и возможности
/// устройства не хранят состояния между вызовами — поля появятся вместе с
/// `acquire`/`configure`/`start` (ADR-08).
#[derive(Debug, Default)]
pub struct AlsaExclusiveBackend;

impl AlsaExclusiveBackend {
    pub fn new() -> AlsaExclusiveBackend {
        AlsaExclusiveBackend
    }

    /// Возможности бэкенда на этой платформе (§2.3, ТЗ-109, ТЗ-110). Модуль
    /// собирается только на Linux, поэтому Exclusive всегда доступен.
    pub fn caps(&self) -> BackendCaps {
        BackendCaps::current()
    }

    /// Перечисление карт ALSA и их PCM-устройств воспроизведения (§6.6,
    /// ADR-24, ТЗ-143): id строится по стабильному `CARD=<id>`, а не по
    /// номеру карты, который может смениться после переподключения
    /// (ТЗ-126). Отказ `card::Iter` — сбой перечисления целиком; отказ
    /// отдельной карты (`Ctl`, `CardInfo`) или устройства без потока
    /// воспроизведения — карта/устройство пропускаются, перечисление
    /// продолжается остальными картами.
    pub fn enumerate(&mut self) -> Result<Vec<HwDeviceInfo>, BackendError> {
        let mut out = Vec::new();
        for card in alsa::card::Iter::new() {
            let card = card.map_err(|e| BackendError::Unavailable(e.to_string()))?;
            let Ok(card_index) = u32::try_from(card.get_index()) else { continue };
            let Ok(ctl) = alsa::ctl::Ctl::from_card(&card, false) else { continue };
            let Ok(card_info) = ctl.card_info() else { continue };
            let Ok(card_id) = card_info.get_id() else { continue };
            let card_id = card_id.to_string();

            for device in alsa::ctl::DeviceIter::new(&ctl) {
                let Ok(device_index) = u32::try_from(device) else { continue };
                let Ok(pcm_info) = ctl.pcm_info(device_index, 0, alsa::Direction::Playback) else {
                    continue;
                };
                let Some(id) = build_hw_id(&card_id, device_index) else { continue };
                let name = pcm_info.get_name().unwrap_or_default().to_string();
                out.push(HwDeviceInfo { id, card: card_index, device: device_index, name });
            }
        }
        Ok(out)
    }

    /// Возможности захваченного устройства (§2.3, §6.6, ТЗ-109, ТЗ-110):
    /// снимаются с PCM, которым владеет `claim`. Пока PCM у потока вывода
    /// (между `start` и `stop`), `claim.pcm_mut()` — `None`; отдельного
    /// варианта `CaptureFailure` для этого состояния спецификация не
    /// задаёт, поэтому используется `DeviceDisconnected` — возможности
    /// снять физически нельзя.
    pub fn device_caps(&mut self, claim: &mut DeviceClaim) -> Result<DeviceCaps, CaptureFailure> {
        let Some(handle) = claim.pcm_mut().as_ref() else {
            return Err(CaptureFailure::DeviceDisconnected);
        };
        Self::device_caps_of(handle.pcm())
    }

    /// Возможности открытого PCM (§2.3, §6.6, ТЗ-109, ТЗ-110): свободная от
    /// захвата функция — `device_caps` делегирует ей, тесты с реальным
    /// устройством могут вызывать её напрямую над `alsa::PCM`.
    pub fn device_caps_of(pcm: &alsa::PCM) -> Result<DeviceCaps, CaptureFailure> {
        let hw = alsa::pcm::HwParams::any(pcm).map_err(driver_refused)?;
        let name = pcm.info().and_then(|info| info.get_name().map(str::to_string)).unwrap_or_default();

        let mut formats = SmallVec::new();
        for (alsa_fmt, out_fmt) in FORMAT_MAP {
            if hw.test_format(alsa_fmt).is_ok() {
                formats.push(out_fmt);
            }
        }

        let ch_min = hw.get_channels_min().map_err(driver_refused)?;
        let ch_max = hw.get_channels_max().map_err(driver_refused)?;
        let mut channels = SmallVec::new();
        for ch in ch_min..=ch_max {
            let Ok(c) = u8::try_from(ch) else { break };
            if hw.test_channels(ch).is_ok() {
                channels.push(c);
            }
        }

        let rates = probe_rates(&hw)?;

        Ok(DeviceCaps { api: OutputApi::AlsaHw, access: AccessKind::Exclusive, name, rates, formats, channels })
    }
}

/// `hw:CARD=<id>,DEV=<d>` для перечисления (§6.6). `None`, если строка не
/// проходит разбор `HwDeviceId` (пустой `card_id` после обрезки пробелов).
fn build_hw_id(card_id: &str, device: u32) -> Option<HwDeviceId> {
    format!("hw:CARD={card_id},DEV={device}").parse().ok()
}

/// Отказ ALSA при чтении hw_params уже открытого PCM (§6.6): единственный
/// подходящий вариант закрытого набора `CaptureFailure`.
fn driver_refused(e: alsa::Error) -> CaptureFailure {
    CaptureFailure::DriverRefused { errno: e.errno() }
}

/// Частоты устройства (§2.3, §6.6): дискретный список через `test_rate` по
/// стандартным частотам, диапазон `get_rate_min`/`get_rate_max` — только
/// если ни одна из них не принята.
fn probe_rates(hw: &alsa::pcm::HwParams<'_>) -> Result<RateSet, CaptureFailure> {
    let mut discrete = SmallVec::new();
    for hz in STANDARD_RATES_HZ {
        if hw.test_rate(hz).is_ok() {
            if let Some(rate) = SampleRate::new(hz) {
                discrete.push(rate);
            }
        }
    }
    if !discrete.is_empty() {
        return Ok(RateSet::Discrete(discrete));
    }

    let min = hw.get_rate_min().map_err(driver_refused)?;
    let max = hw.get_rate_max().map_err(driver_refused)?;
    match (SampleRate::new(min), SampleRate::new(max)) {
        (Some(min), Some(max)) => Ok(RateSet::Range { min, max }),
        // Драйвер вернул 0 Гц: не отказ ALSA, но представить диапазон нечем.
        // errno = 0 отличает этот случай от настоящей ошибки драйвера.
        _ => Err(CaptureFailure::DriverRefused { errno: 0 }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    #[test]
    fn caps_reports_exclusive_on_linux() {
        let backend = AlsaExclusiveBackend::new();
        assert!(backend.caps().exclusive);
    }

    #[test]
    fn build_hw_id_formats_card_and_device() {
        let id = build_hw_id("PCH", 1).expect("valid id");
        assert_eq!(id.as_str(), "hw:CARD=PCH,DEV=1");
    }

    #[test]
    fn format_map_covers_all_output_sample_formats() {
        assert_eq!(FORMAT_MAP.len(), 5);
        assert!(FORMAT_MAP.contains(&(alsa::pcm::Format::S16LE, OutputSampleFormat::S16Le)));
        assert!(FORMAT_MAP.contains(&(alsa::pcm::Format::FloatLE, OutputSampleFormat::F32)));
    }
}
