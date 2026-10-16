//! Опрос аудиосервера ОС для этапа «Уровень ОС» (§6.26, ADR-21, ТЗ-77, ОВС-9).
//!
//! `OsLevelProbe::probe` дёргается движком один раз на каждое открытие
//! Shared-потока, вне UI (ADR-21). Спецификация §6.26 задаёт сигнатуру
//! `probe(&mut self, dev)`; здесь добавлен параметр `stream_rate` — шаг 2
//! алгоритма §6.26 сравнивает частоту потока со списком `allowed-rates`
//! аудиосервера, и без неё чистую функцию `os_stage_from_pw_settings`
//! выделить было бы нельзя.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::audio::format::SampleRate;
use crate::audio::path::transform::{Confidence, ConfirmedBy};
use crate::audio::path::{OsServer, OsStage};
use crate::settings::playback::SharedDeviceChoice;

/// Опрос аудиосервера ОС по факту открытого Shared-потока (§6.26, ТЗ-77,
/// ОВС-9). Реализации делают I/O (вызов `pw-metadata` и т. п.), поэтому
/// движок держит пробу за трейтом и вызывает её вне аудио-колбэка.
pub trait OsLevelProbe: Send {
    fn probe(&mut self, dev: &SharedDeviceChoice, stream_rate: SampleRate) -> OsStage;
}

/// Таймаут ожидания `pw-metadata` на Linux (§6.26): процесс, который не
/// успел ответить за это время, считается недоступным и убивается.
const PW_METADATA_TIMEOUT: Duration = Duration::from_millis(500);

/// Боевая реализация `OsLevelProbe` (§6.26, ADR-21, ТЗ-77). Exclusive —
/// забота движка (он не вызывает пробу для Exclusive-потока, т. к. ОС в
/// тракте не участвует), здесь это не проверяется.
#[derive(Default)]
pub struct SystemOsProbe;

impl OsLevelProbe for SystemOsProbe {
    #[cfg(target_os = "linux")]
    fn probe(&mut self, _dev: &SharedDeviceChoice, stream_rate: SampleRate) -> OsStage {
        let Some(text) = run_pw_metadata() else {
            return OsStage::Unknown;
        };
        os_stage_from_pw_settings(&text, stream_rate)
    }

    #[cfg(target_os = "windows")]
    fn probe(&mut self, _dev: &SharedDeviceChoice, _stream_rate: SampleRate) -> OsStage {
        use cpal::traits::{DeviceTrait, HostTrait};
        // §6.26: выбор конкретного устройства (`dev`) не учитывается — опрашивается
        // устройство вывода по умолчанию (упрощение этого шага, помечено в задании).
        let Some(device) = cpal::default_host().default_output_device() else {
            return OsStage::Unknown;
        };
        let Ok(config) = device.default_output_config() else {
            return OsStage::Unknown;
        };
        let Some(rate) = SampleRate::new(config.sample_rate().0) else {
            return OsStage::Unknown;
        };
        OsStage::Known {
            server: OsServer::WasapiMixer,
            rate: Some(rate),
            format: Some(format!("{:?}", config.sample_format())),
            confidence: Confidence::Confirmed(ConfirmedBy::OsQuery),
        }
    }

    #[cfg(target_os = "macos")]
    fn probe(&mut self, _dev: &SharedDeviceChoice, _stream_rate: SampleRate) -> OsStage {
        // `OsServer` не содержит варианта CoreAudio (§3.2); до его добавления
        // номинальная частота устройства не может быть представлена как
        // `Known` — отдаём `Unknown`.
        OsStage::Unknown
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    fn probe(&mut self, _dev: &SharedDeviceChoice, _stream_rate: SampleRate) -> OsStage {
        OsStage::Unknown
    }
}

/// Запуск `pw-metadata -n settings` с таймаутом `PW_METADATA_TIMEOUT`
/// (§6.26). `None` при отсутствии процесса, таймауте, нечитаемом выводе или
/// ошибке запуска.
#[cfg(target_os = "linux")]
fn run_pw_metadata() -> Option<String> {
    let mut child = Command::new("pw-metadata")
        .args(["-n", "settings"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() >= PW_METADATA_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }

    let output = child.wait_with_output().ok()?;
    String::from_utf8(output.stdout).ok()
}

/// Фейк для тестов движка (ADR-20): отдаёт заранее заданный `OsStage` и
/// считает число вызовов `probe` (§6.26).
pub struct FakeOsProbe {
    result: OsStage,
    pub calls: usize,
}

impl FakeOsProbe {
    pub fn new(result: OsStage) -> FakeOsProbe {
        FakeOsProbe { result, calls: 0 }
    }
}

impl OsLevelProbe for FakeOsProbe {
    fn probe(&mut self, _dev: &SharedDeviceChoice, _stream_rate: SampleRate) -> OsStage {
        self.calls += 1;
        self.result.clone()
    }
}

/// Разбор вывода `pw-metadata -n settings` в этап «Уровень ОС» (§6.26, ТЗ-77,
/// ОВС-9). Чистая функция: без I/O, вызывается реализацией `OsLevelProbe`
/// поверх уже прочитанного текста.
///
/// Алгоритм §6.26:
/// - `clock.force-rate` ≠ 0 — граф принудительно работает на этой частоте:
///   `rate = force-rate`, подтверждено запросом к ОС (`Confirmed(OsQuery)`
///   — сама частота прочитана у ОС).
/// - иначе, если `stream_rate` входит в `clock.allowed-rates` — граф может
///   переключиться на частоту потока: `rate = stream_rate`, достоверность
///   `Assumed` (переключение не подтверждено, только разрешено).
/// - иначе граф пересчитывает поток к `clock.rate`: `rate = clock.rate`,
///   `Confirmed(OsQuery)`.
/// - `clock.allowed-rates` нет в выводе — считается, что разрешена только
///   `clock.rate`.
/// - `clock.rate` не найден или вывод не разбирается — `OsStage::Unknown`.
pub fn os_stage_from_pw_settings(text: &str, stream_rate: SampleRate) -> OsStage {
    let Some(clock_rate) = find_rate(text, "clock.rate") else {
        return OsStage::Unknown;
    };
    let force_rate = find_rate(text, "clock.force-rate");
    if let Some(force) = force_rate {
        if force.hz() != 0 {
            return OsStage::Known {
                server: OsServer::PipeWire,
                rate: Some(force),
                format: None,
                confidence: Confidence::Confirmed(ConfirmedBy::OsQuery),
            };
        }
    }

    let allowed = find_allowed_rates(text).unwrap_or_else(|| vec![clock_rate]);
    if allowed.contains(&stream_rate) {
        return OsStage::Known {
            server: OsServer::PipeWire,
            rate: Some(stream_rate),
            format: None,
            confidence: Confidence::Assumed,
        };
    }

    OsStage::Known {
        server: OsServer::PipeWire,
        rate: Some(clock_rate),
        format: None,
        confidence: Confidence::Confirmed(ConfirmedBy::OsQuery),
    }
}

/// Значение ключа `key:'<name>'` как частота (например, `clock.rate` или
/// `clock.force-rate`). `None`, если ключ не найден или не разбирается.
fn find_rate(text: &str, name: &str) -> Option<SampleRate> {
    let value = find_value(text, name)?;
    let hz: u32 = value.trim().parse().ok()?;
    SampleRate::new(hz)
}

/// Значение ключа `clock.allowed-rates` вида `[ 44100 48000 ]` (пробелы или
/// запятые) как список частот. `None`, если ключ не найден.
fn find_allowed_rates(text: &str) -> Option<Vec<SampleRate>> {
    let value = find_value(text, "clock.allowed-rates")?;
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    let rates: Vec<SampleRate> = inner
        .split([',', ' '])
        .filter(|tok| !tok.trim().is_empty())
        .filter_map(|tok| tok.trim().parse::<u32>().ok())
        .filter_map(SampleRate::new)
        .collect();
    Some(rates)
}

/// Значение ключа `key:'<name>'` в строке вида
/// `update: id:0 key:'clock.rate' value:'48000' type:''`.
fn find_value<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("key:'{name}'");
    for line in text.lines() {
        let Some(key_pos) = line.find(&needle) else { continue };
        let after_key = &line[key_pos + needle.len()..];
        let value_marker = "value:'";
        let value_pos = after_key.find(value_marker)?;
        let after_marker = &after_key[value_pos + value_marker.len()..];
        let end = after_marker.find('\'')?;
        return Some(&after_marker[..end]);
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    const REAL_SAMPLE: &str = r#"Found "settings" metadata 31
update: id:0 key:'log.level' value:'2' type:''
update: id:0 key:'clock.rate' value:'48000' type:''
update: id:0 key:'clock.allowed-rates' value:'[ 48000 ]' type:''
update: id:0 key:'clock.quantum' value:'1024' type:''
update: id:0 key:'clock.force-rate' value:'0' type:''
"#;

    fn r(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    #[test]
    fn force_rate_wins() {
        let text = r#"update: id:0 key:'clock.rate' value:'48000' type:''
update: id:0 key:'clock.allowed-rates' value:'[ 48000 ]' type:''
update: id:0 key:'clock.force-rate' value:'96000' type:''
"#;
        let stage = os_stage_from_pw_settings(text, r(48_000));
        assert_eq!(
            stage,
            OsStage::Known {
                server: OsServer::PipeWire,
                rate: Some(r(96_000)),
                format: None,
                confidence: Confidence::Confirmed(ConfirmedBy::OsQuery),
            }
        );
    }

    #[test]
    fn stream_rate_in_allowed_is_assumed() {
        let stage = os_stage_from_pw_settings(REAL_SAMPLE, r(48_000));
        assert_eq!(
            stage,
            OsStage::Known {
                server: OsServer::PipeWire,
                rate: Some(r(48_000)),
                format: None,
                confidence: Confidence::Assumed,
            }
        );
    }

    #[test]
    fn other_rate_resampled_to_clock_rate() {
        let stage = os_stage_from_pw_settings(REAL_SAMPLE, r(96_000));
        assert_eq!(
            stage,
            OsStage::Known {
                server: OsServer::PipeWire,
                rate: Some(r(48_000)),
                format: None,
                confidence: Confidence::Confirmed(ConfirmedBy::OsQuery),
            }
        );
    }

    #[test]
    fn multiple_allowed_rates_parsed() {
        let text = r#"update: id:0 key:'clock.rate' value:'48000' type:''
update: id:0 key:'clock.allowed-rates' value:'[ 44100, 48000, 88200, 96000 ]' type:''
update: id:0 key:'clock.force-rate' value:'0' type:''
"#;
        let stage = os_stage_from_pw_settings(text, r(88_200));
        assert_eq!(
            stage,
            OsStage::Known {
                server: OsServer::PipeWire,
                rate: Some(r(88_200)),
                format: None,
                confidence: Confidence::Assumed,
            }
        );
    }

    #[test]
    fn garbage_is_unknown() {
        assert_eq!(os_stage_from_pw_settings("", r(48_000)), OsStage::Unknown);
        assert_eq!(os_stage_from_pw_settings("Found nothing", r(48_000)), OsStage::Unknown);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_probe_never_panics() {
        let mut probe = SystemOsProbe;
        let stage = probe.probe(&SharedDeviceChoice::SystemDefault, r(48_000));
        match stage {
            OsStage::Known { server: OsServer::PipeWire, .. } | OsStage::Unknown => {}
            other => panic!("unexpected stage without pw-metadata semantics: {other:?}"),
        }
    }
}
