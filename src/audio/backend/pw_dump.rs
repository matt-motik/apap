//! Разбор вывода `pw-dump` в список узлов PipeWire `Audio/Sink` и запуск
//! самого процесса с таймаутом (AM1.0-B37, ADR-07 «Уточнение»,
//! `docs/01_audio_modes_v1.0/03_spec.md#adr-07-бэкенды-вывода»). `parse_sinks`
//! — чистая функция без I/O; `enumerate_pipewire_sinks` запускает `pw-dump`
//! (паттерн `run_pw_metadata`, `src/audio/backend/os_probe.rs`) и разбирает
//! его вывод. Сборка `SharedDeviceInfo` для этих узлов и решение «запускать
//! только когда сервер — PipeWire» — у вызывающей стороны
//! (`src/audio/backend/shared.rs`), не здесь: этот файл не знает о
//! `SharedDeviceInfo` и об активном аудиосервере.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::collections::HashMap;
use std::fmt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

/// Узел PipeWire `Audio/Sink`, как его отдаёт `pw-dump` (ADR-07):
/// подмножество `info.props`, которого хватает для подписи устройства и
/// для открытия Shared-потока по имени `pipewire:NODE=<node.name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PwSink {
    /// `node.name` — стабильная строка, не меняющаяся между перезапусками
    /// PipeWire (ADR-07): сохраняется в настройках и подставляется в
    /// `pipewire:NODE=<node.name>`. Единственное обязательное поле: узел
    /// без него нельзя ни сохранить, ни открыть.
    pub node_name: String,
    /// `node.description` — резервная подпись, когда узел не сопоставлен
    /// карте ALSA (виртуальный sink, Bluetooth-узел без карты; ADR-07).
    pub node_description: Option<String>,
    /// `alsa.card_name` — первый аргумент `hw_device_label` (ADR-07,
    /// `catalog.rs`), если узел сопоставлен карте ALSA, которой
    /// распоряжается WirePlumber. Отсутствует у виртуальных/сетевых sink.
    pub alsa_card_name: Option<String>,
    /// Номер карты ALSA узла: `alsa.card`, а если его нет — `api.alsa.pcm.card`
    /// (оба встречаются в реальных выводах `pw-dump` в зависимости от версии
    /// WirePlumber). Нужен будущему сопоставлению узла с `HwDeviceId`
    /// каталога (ADR-07); в этом шаге только сохраняется.
    pub alsa_card: Option<u32>,
    /// `device.profile.description` — подпись активного профиля карты
    /// (аналог `pcm_name` у `hw_device_label`, например «Analog Stereo»,
    /// «Multichannel», «Digital»), если она информативнее общего
    /// `node.description`.
    pub device_profile_description: Option<String>,
}

/// Вывод `pw-dump` не разобрался как список узлов PipeWire (ADR-07): пустой
/// или битый JSON, либо верхний уровень не массив объектов. Вызывающая
/// сторона (следующий шаг) реагирует так же, как на отказ самого процесса —
/// списком только «Системное по умолчанию» и записью в журнал (честность
/// сигнала важнее полноты списка).
#[derive(Debug)]
pub struct PwDumpError(String);

impl fmt::Display for PwDumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pw-dump output did not parse as JSON: {}", self.0)
    }
}

impl std::error::Error for PwDumpError {}

/// Один объект верхнего уровня вывода `pw-dump --color=never` (массив JSON).
/// Поля вне `type`/`info.props` неизвестной формы игнорируются серд по
/// умолчанию — схему `pw-dump` спецификация не фиксирует целиком.
#[derive(Deserialize)]
struct PwDumpObject {
    #[serde(rename = "type")]
    kind: Option<String>,
    info: Option<PwDumpInfo>,
}

#[derive(Deserialize)]
struct PwDumpInfo {
    props: Option<HashMap<String, Value>>,
}

/// Тип объекта `pw-dump`, который описывает узел графа PipeWire (а не порт,
/// устройство ALSA, модуль и т. п.).
const NODE_TYPE: &str = "PipeWire:Interface:Node";

/// Класс медиа узла-выхода звука. Узлы с другим `media.class`
/// (`Audio/Source` — в том числе мониторы sink'ов, которые PipeWire всегда
/// публикует этим классом, а не отдельным «монитор»-флагом у `Audio/Sink`;
/// `Audio/Duplex`; видео и т. п.) отсеиваются этой же проверкой — отдельного
/// правила для мониторов не нужно.
const SINK_MEDIA_CLASS: &str = "Audio/Sink";

/// Разбор вывода `pw-dump` в список узлов `Audio/Sink` (§ ADR-07,
/// AM1.0-B37).
///
/// Правило отсечения: из всех объектов верхнего уровня остаются только
/// `type: "PipeWire:Interface:Node"` с `info.props."media.class" ==
/// "Audio/Sink"` и непустым `node.name`. Это молча убирает: объекты другого
/// типа (порты, устройства, модули, клиенты), узлы-источники и мониторы
/// (другой `media.class`), и узлы без `node.name` — такой узел нельзя ни
/// сохранить в настройках, ни открыть как `pipewire:NODE=<node.name>`, он
/// бесполезен для этой задачи независимо от причины отсутствия поля.
/// Программные/виртуальные sink (Bluetooth, null-sink и подобные) **не**
/// отсекаются: они остаются в списке без заполненных `alsa_*`-полей и
/// получат подпись `node.description` на следующем шаге (ADR-07, тот же
/// принцип, что уже применяется к Shared-устройствам без карты ALSA).
///
/// Ошибка возвращается только когда верхний уровень вывода не разбирается
/// как JSON-массив объектов такой формы (включая случай, когда это
/// JSON-объект, а не массив, или вообще не JSON); отдельный битый элемент
/// массива делает битым весь разбор — `pw-dump` либо пишет валидный JSON,
/// либо не отвечает совсем, промежуточного «частично битый массив» в
/// реальном выводе не бывает.
pub fn parse_sinks(json: &str) -> Result<Vec<PwSink>, PwDumpError> {
    let objects: Vec<PwDumpObject> =
        serde_json::from_str(json).map_err(|err| PwDumpError(err.to_string()))?;

    let mut sinks = Vec::with_capacity(objects.len());
    for object in objects {
        if object.kind.as_deref() != Some(NODE_TYPE) {
            continue;
        }
        let Some(props) = object.info.and_then(|info| info.props) else {
            continue;
        };
        if prop_str(&props, "media.class") != Some(SINK_MEDIA_CLASS) {
            continue;
        }
        let Some(node_name) = prop_str(&props, "node.name") else {
            continue;
        };

        let alsa_card = prop_str(&props, "alsa.card")
            .or_else(|| prop_str(&props, "api.alsa.pcm.card"))
            .and_then(|value| value.parse().ok());

        sinks.push(PwSink {
            node_name: node_name.to_string(),
            node_description: prop_str(&props, "node.description").map(str::to_string),
            alsa_card_name: prop_str(&props, "alsa.card_name").map(str::to_string),
            alsa_card,
            device_profile_description: prop_str(&props, "device.profile.description")
                .map(str::to_string),
        });
    }
    Ok(sinks)
}

/// Строковое значение свойства узла, если оно есть и правда строка.
fn prop_str<'a>(props: &'a HashMap<String, Value>, key: &str) -> Option<&'a str> {
    props.get(key).and_then(Value::as_str)
}

/// Таймаут ожидания `pw-dump`: тот же порог, что и `pw-metadata`
/// (`PW_METADATA_TIMEOUT`, `src/audio/backend/os_probe.rs`, §6.26) — ADR-07
/// «Уточнение» требует одинаковую реакцию на оба отказа (список Совместимого
/// — только «Системное по умолчанию» + запись в журнал).
const PW_DUMP_TIMEOUT: Duration = Duration::from_millis(500);

/// Причина, по которой `pw-dump` не вернул текст (ADR-07 «Уточнение»,
/// AM1.0-B37): бинарь не найден/не запустился, процесс не ответил за
/// `PW_DUMP_TIMEOUT` и был убит, завершился с ненулевым кодом выхода, или
/// его вывод не разобрался как UTF-8. Разбор уже прочитанного текста в узлы
/// — отдельная ошибка (`PwDumpError`, выше): здесь только об исполнении
/// самого процесса.
#[derive(Debug)]
pub enum PwDumpRunError {
    /// `Command::spawn` вернул ошибку — обычный случай десктопа без
    /// установленного `pipewire-utils`/`pw-dump`.
    Spawn,
    /// Процесс не ответил за `PW_DUMP_TIMEOUT` (или опрос `try_wait` сам
    /// вернул ошибку ОС — редкий случай, реакция та же).
    Timeout,
    /// Процесс завершился, но с ненулевым кодом выхода.
    ExitFailure,
    /// Вывод процесса не в UTF-8.
    NotUtf8,
}

impl fmt::Display for PwDumpRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            PwDumpRunError::Spawn => "pw-dump did not start",
            PwDumpRunError::Timeout => "pw-dump did not respond in time",
            PwDumpRunError::ExitFailure => "pw-dump exited with a non-zero status",
            PwDumpRunError::NotUtf8 => "pw-dump output was not UTF-8",
        };
        write!(f, "{text}")
    }
}

impl std::error::Error for PwDumpRunError {}

/// Отказ перечисления per-sink узлов PipeWire — на этапе запуска процесса
/// или на этапе разбора его вывода (ADR-07 «Уточнение», AM1.0-B37).
#[derive(Debug)]
pub enum PwDumpSinkError {
    Run(PwDumpRunError),
    Parse(PwDumpError),
}

impl fmt::Display for PwDumpSinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PwDumpSinkError::Run(err) => write!(f, "{err}"),
            PwDumpSinkError::Parse(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PwDumpSinkError {}

/// Абстракция над процессом для чистого тестирования цикла ожидания с
/// таймаутом (AM1.0-B37, шаг 3): `poll` — аналог `Child::try_wait` (`Ok(true)`
/// — завершился, `Ok(false)` — ещё работает, `Err` — опрос сам отказал),
/// `kill` — аналог `Child::kill`, `elapsed`/`sleep_tick` внедряются, чтобы
/// тест промотал время без реального `thread::sleep` и не ждал его.
trait TimedProcess {
    fn poll(&mut self) -> Result<bool, ()>;
    fn kill(&mut self);
    fn elapsed(&self) -> Duration;
    fn sleep_tick(&mut self);
}

/// Чистый цикл «жди, пока процесс не завершится, или не истечёт `timeout`»
/// (паттерн `run_pw_metadata`, `src/audio/backend/os_probe.rs`): убивает
/// процесс при превышении таймаута. `true` — процесс завершился сам за
/// отведённое время; `false` — таймаут (с убийством процесса) или ошибка
/// опроса — в обоих случаях вызывающая сторона не читает вывод.
fn wait_for_exit(process: &mut dyn TimedProcess, timeout: Duration) -> bool {
    loop {
        match process.poll() {
            Ok(true) => return true,
            Ok(false) => {
                if process.elapsed() >= timeout {
                    process.kill();
                    return false;
                }
                process.sleep_tick();
            }
            Err(()) => return false,
        }
    }
}

/// Боевой `TimedProcess` поверх `std::process::Child` (AM1.0-B37, шаг 3).
struct ChildProcess<'a> {
    child: &'a mut std::process::Child,
    start: Instant,
}

impl TimedProcess for ChildProcess<'_> {
    fn poll(&mut self) -> Result<bool, ()> {
        match self.child.try_wait() {
            Ok(Some(_status)) => Ok(true),
            Ok(None) => Ok(false),
            Err(_) => Err(()),
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep_tick(&mut self) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Запуск `pw-dump` с таймаутом `PW_DUMP_TIMEOUT`, вне UI-потока (вызывающая
/// сторона — перечисление Shared-устройств в движке/воркере, не UI, ADR-07
/// «Уточнение»). Паттерн таймаута — `run_pw_metadata` (`os_probe.rs`); сам
/// цикл ожидания — чистая `wait_for_exit` выше, протестированная без
/// реального процесса (AM1.0-B37, шаг 3).
fn run_pw_dump() -> Result<String, PwDumpRunError> {
    let mut child = Command::new("pw-dump")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| PwDumpRunError::Spawn)?;

    let start = Instant::now();
    let mut process = ChildProcess { child: &mut child, start };
    if !wait_for_exit(&mut process, PW_DUMP_TIMEOUT) {
        return Err(PwDumpRunError::Timeout);
    }

    let output = child.wait_with_output().map_err(|_| PwDumpRunError::Spawn)?;
    if !output.status.success() {
        return Err(PwDumpRunError::ExitFailure);
    }
    String::from_utf8(output.stdout).map_err(|_| PwDumpRunError::NotUtf8)
}

/// Запуск `run` (боевой — `run_pw_dump`, в тестах — фейковый замыкание) и
/// разбор его вывода в список узлов `Audio/Sink` одной функцией (AM1.0-B37,
/// шаг 3): отдельно от `enumerate_pipewire_sinks`, чтобы связку «ошибка
/// запуска» / «ошибка разбора» можно было протестировать без реального
/// процесса.
fn enumerate_pipewire_sinks_with(
    run: impl FnOnce() -> Result<String, PwDumpRunError>,
) -> Result<Vec<PwSink>, PwDumpSinkError> {
    let text = run().map_err(PwDumpSinkError::Run)?;
    parse_sinks(&text).map_err(PwDumpSinkError::Parse)
}

/// Перечисление per-sink узлов PipeWire `Audio/Sink` через `pw-dump`: запуск
/// процесса с таймаутом + разбор вывода, одной функцией для вызывающего
/// (`src/audio/backend/shared.rs`, ADR-07 «Уточнение», AM1.0-B37). Решение
/// «запускать только когда активный сервер — PipeWire» — у вызывающей
/// стороны (этот файл не знает про `active_server_name`).
pub fn enumerate_pipewire_sinks() -> Result<Vec<PwSink>, PwDumpSinkError> {
    enumerate_pipewire_sinks_with(run_pw_dump)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    /// Фикстура в форме реального `pw-dump`: MOTU M2 (сопоставлен карте
    /// ALSA), два профиля карты Asus Xonar (Multichannel/Digital — тот же
    /// `alsa.card_name`, разные `node.name`/`device.profile.description`,
    /// как в реальной многопрофильной карте), Bluetooth-sink без ALSA,
    /// монитор-источник того же sink (другой `media.class` — должен быть
    /// отсеян) и порт графа (другой `type` — тоже отсеян).
    const FIXTURE: &str = r#"[
        {
            "id": 45,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Sink",
                    "node.name": "alsa_output.usb-MOTU_M2_M2AE0B83N9-00.HiFi__Line__sink.3",
                    "node.description": "MOTU M2 Analog Stereo",
                    "alsa.card_name": "USB Audio",
                    "alsa.card": "1",
                    "api.alsa.pcm.card": "1",
                    "device.profile.description": "Analog Stereo"
                }
            }
        },
        {
            "id": 52,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Sink",
                    "node.name": "alsa_output.pci-0000_07_00.0.multichannel-output",
                    "node.description": "Asus Xonar Multichannel Output",
                    "alsa.card_name": "Xonar DSX",
                    "alsa.card": "2",
                    "device.profile.description": "Multichannel Output"
                }
            }
        },
        {
            "id": 53,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Sink",
                    "node.name": "alsa_output.pci-0000_07_00.0.iec958-stereo",
                    "node.description": "Asus Xonar Digital Output",
                    "alsa.card_name": "Xonar DSX",
                    "alsa.card": "2",
                    "device.profile.description": "Digital Output (S/PDIF)"
                }
            }
        },
        {
            "id": 61,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Sink",
                    "node.name": "bluez_output.AA_BB_CC_DD_EE_FF.1"
                }
            }
        },
        {
            "id": 62,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Source",
                    "node.name": "alsa_output.usb-MOTU_M2_M2AE0B83N9-00.HiFi__Line__sink.3.monitor",
                    "node.description": "Monitor of MOTU M2 Analog Stereo"
                }
            }
        },
        {
            "id": 63,
            "type": "PipeWire:Interface:Port",
            "info": {
                "props": {
                    "media.class": "Audio/Sink",
                    "node.name": "should-be-ignored-not-a-node"
                }
            }
        },
        {
            "id": 64,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Sink",
                    "node.description": "No node.name, unusable"
                }
            }
        }
    ]"#;

    #[test]
    fn parses_motu_and_xonar_sinks() {
        let sinks = parse_sinks(FIXTURE).unwrap();
        // MOTU, Xonar Multichannel, Xonar Digital, Bluetooth — именно эти
        // четыре; монитор-источник, порт и узел без node.name отсеяны.
        assert_eq!(sinks.len(), 4, "sinks: {sinks:?}");

        let motu = &sinks[0];
        assert_eq!(
            motu.node_name,
            "alsa_output.usb-MOTU_M2_M2AE0B83N9-00.HiFi__Line__sink.3"
        );
        assert_eq!(motu.node_description.as_deref(), Some("MOTU M2 Analog Stereo"));
        assert_eq!(motu.alsa_card_name.as_deref(), Some("USB Audio"));
        assert_eq!(motu.alsa_card, Some(1));
        assert_eq!(motu.device_profile_description.as_deref(), Some("Analog Stereo"));

        let multichannel = &sinks[1];
        assert_eq!(multichannel.alsa_card_name.as_deref(), Some("Xonar DSX"));
        assert_eq!(multichannel.alsa_card, Some(2));
        assert_eq!(
            multichannel.device_profile_description.as_deref(),
            Some("Multichannel Output")
        );

        let digital = &sinks[2];
        assert_eq!(digital.alsa_card_name.as_deref(), Some("Xonar DSX"));
        assert_eq!(digital.alsa_card, Some(2));
        assert_eq!(digital.device_profile_description.as_deref(), Some("Digital Output (S/PDIF)"));
        assert_ne!(multichannel.node_name, digital.node_name);
    }

    #[test]
    fn non_alsa_sink_has_no_alsa_fields_but_is_kept() {
        let sinks = parse_sinks(FIXTURE).unwrap();
        let bluetooth = sinks
            .iter()
            .find(|sink| sink.node_name == "bluez_output.AA_BB_CC_DD_EE_FF.1")
            .expect("bluetooth sink must not be dropped");
        assert_eq!(bluetooth.alsa_card_name, None);
        assert_eq!(bluetooth.alsa_card, None);
        assert_eq!(bluetooth.node_description, None);
    }

    #[test]
    fn monitor_source_and_port_and_nameless_node_are_skipped() {
        let sinks = parse_sinks(FIXTURE).unwrap();
        assert!(!sinks.iter().any(|sink| sink.node_name.ends_with(".monitor")));
        assert!(!sinks.iter().any(|sink| sink.node_name == "should-be-ignored-not-a-node"));
        assert!(sinks.iter().all(|sink| !sink.node_name.is_empty()));
    }

    #[test]
    fn empty_array_is_empty_list() {
        assert_eq!(parse_sinks("[]").unwrap(), Vec::new());
    }

    #[test]
    fn alsa_card_falls_back_to_api_alsa_pcm_card() {
        let json = r#"[{
            "type": "PipeWire:Interface:Node",
            "info": { "props": {
                "media.class": "Audio/Sink",
                "node.name": "fallback-card-number",
                "api.alsa.pcm.card": "7"
            }}
        }]"#;
        let sinks = parse_sinks(json).unwrap();
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].alsa_card, Some(7));
    }

    #[test]
    fn not_json_is_err() {
        assert!(parse_sinks("not json at all").is_err());
        assert!(parse_sinks("").is_err());
    }

    #[test]
    fn top_level_object_instead_of_array_is_err() {
        assert!(parse_sinks(r#"{"type": "PipeWire:Interface:Node"}"#).is_err());
    }

    #[test]
    fn truncated_json_is_err() {
        assert!(parse_sinks(r#"[{"type": "PipeWire:Interface:Node", "info": {"#).is_err());
    }

    #[test]
    fn wrong_type_for_known_field_is_err() {
        // `type` должно быть строкой; число делает битым весь элемент, а
        // значит и весь разбор (см. doc-комментарий `parse_sinks`).
        let json = r#"[{"type": 123, "info": {"props": {"media.class": "Audio/Sink", "node.name": "x"}}}]"#;
        assert!(parse_sinks(json).is_err());
    }

    /// Фейковый `TimedProcess` (AM1.0-B37, шаг 3): очередь ответов `poll` и
    /// очередь значений `elapsed`, которую двигает только `sleep_tick` — так
    /// тест различает «ушло время между двумя опросами» от «секундомер не
    /// сдвинулся», без реального `thread::sleep`.
    #[derive(Default)]
    struct FakeTimedProcess {
        poll_results: std::collections::VecDeque<Result<bool, ()>>,
        elapsed_ticks: std::collections::VecDeque<Duration>,
        killed: bool,
        sleeps: usize,
    }

    impl TimedProcess for FakeTimedProcess {
        fn poll(&mut self) -> Result<bool, ()> {
            self.poll_results.pop_front().unwrap_or(Ok(true))
        }

        fn kill(&mut self) {
            self.killed = true;
        }

        fn elapsed(&self) -> Duration {
            self.elapsed_ticks.front().copied().unwrap_or(Duration::ZERO)
        }

        fn sleep_tick(&mut self) {
            self.sleeps += 1;
            self.elapsed_ticks.pop_front();
        }
    }

    #[test]
    fn wait_for_exit_true_when_process_exits_immediately() {
        let mut process = FakeTimedProcess { poll_results: [Ok(true)].into(), ..Default::default() };
        assert!(wait_for_exit(&mut process, Duration::from_millis(500)));
        assert!(!process.killed);
        assert_eq!(process.sleeps, 0);
    }

    #[test]
    fn wait_for_exit_kills_and_returns_false_on_timeout() {
        let mut process = FakeTimedProcess {
            poll_results: [Ok(false)].into(),
            elapsed_ticks: [Duration::from_millis(600)].into(),
            ..Default::default()
        };
        assert!(!wait_for_exit(&mut process, Duration::from_millis(500)));
        assert!(process.killed);
        assert_eq!(process.sleeps, 0, "elapsed >= timeout must kill before sleeping again");
    }

    #[test]
    fn wait_for_exit_true_after_a_few_not_yet_timed_out_polls() {
        let mut process = FakeTimedProcess {
            poll_results: [Ok(false), Ok(false), Ok(true)].into(),
            elapsed_ticks: [Duration::from_millis(100), Duration::from_millis(200)].into(),
            ..Default::default()
        };
        assert!(wait_for_exit(&mut process, Duration::from_millis(500)));
        assert!(!process.killed);
        assert_eq!(process.sleeps, 2);
    }

    #[test]
    fn wait_for_exit_false_on_poll_error_without_killing() {
        // Ошибка самого опроса (`try_wait` вернул `Err`) — не таймаут, убивать
        // уже неотвечающий дескриптор процесса нечем (см. doc-комментарий
        // `TimedProcess::poll`); вызывающая сторона просто не читает вывод.
        let mut process = FakeTimedProcess { poll_results: [Err(())].into(), ..Default::default() };
        assert!(!wait_for_exit(&mut process, Duration::from_millis(500)));
        assert!(!process.killed);
    }

    #[test]
    fn enumerate_pipewire_sinks_with_propagates_run_error() {
        let result = enumerate_pipewire_sinks_with(|| Err(PwDumpRunError::Timeout));
        match result {
            Err(PwDumpSinkError::Run(PwDumpRunError::Timeout)) => {}
            other => panic!("expected Run(Timeout), got {other:?}"),
        }
    }

    #[test]
    fn enumerate_pipewire_sinks_with_propagates_parse_error() {
        let result = enumerate_pipewire_sinks_with(|| Ok("not json".to_string()));
        match result {
            Err(PwDumpSinkError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn enumerate_pipewire_sinks_with_returns_parsed_sinks_on_success() {
        let result = enumerate_pipewire_sinks_with(|| Ok(FIXTURE.to_string()));
        let sinks = result.expect("run + parse ok");
        assert_eq!(sinks.len(), 4);
    }

    #[test]
    fn pw_dump_run_error_display_is_not_empty() {
        for err in [
            PwDumpRunError::Spawn,
            PwDumpRunError::Timeout,
            PwDumpRunError::ExitFailure,
            PwDumpRunError::NotUtf8,
        ] {
            assert!(!err.to_string().is_empty());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn run_pw_dump_never_panics() {
        // Не проверяет содержимое (зависит от того, установлен ли `pw-dump`
        // на машине теста) — только что вызов не паникует и укладывается в
        // обычное время теста (AM1.0-B37, шаг 3; `PW_DUMP_TIMEOUT` = 500 мс).
        let _ = run_pw_dump();
    }
}
