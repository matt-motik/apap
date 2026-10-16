//! Отчёт окна «Путь сигнала»: шесть этапов, рекомендации и граница
//! гарантии (AM1.0 §3.6, §6.25, ТЗ-70…ТЗ-79, ТЗ-129, ТЗ-131, ОВС-8, ОВС-11).
//!
//! `build_report` — чистая функция из уже готовых данных (`SignalPath` или
//! `Blocked`, `DynamicPathState`, `ModeSettings`): без I/O и без обращения к
//! движку или бэкенду (ТЗ-79).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use smallvec::SmallVec;

use crate::audio::backend::{AccessKind, DeviceBufferActual, DriverFormat, OutputApi};
use crate::audio::error::Incompatibility;
use crate::audio::format::{Codec, OutputSampleFormat, SourceFormat, SourceKind, StreamFormat};
use crate::settings::params::PARAMS;
use crate::settings::playback::{DsdFilter, ModeKind, ModeSettings};

use super::recommend::{recommend, PathSubject, Recommendation};
use super::signal::{DynamicPathState, Md5Status, NotCheckedWhy, RtPriorityState, SignalPath};
use super::transform::{dsd_name, khz, Cause, Confidence, ConfirmedBy, Transform, TransformKind};
use super::{
    Blocked, DecoderStage, DeviceStage, OsServer, OsStage, OutputStage, ProcessingStage, SourceStage, StageId,
    StageStatus,
};

/// Граница гарантии (ТЗ-78): показывается во всех режимах.
pub const GUARANTEE_BOUNDARY: &str = "Плеер отвечает за данные до входа драйвера устройства. \
Что делает с сигналом сам ЦАП (внутренний ресемплинг, цифровая громкость, DSP), плеер не контролирует.";

/// Порядок этапов (ТЗ-70).
const STAGE_ORDER: [StageId; 6] = [
    StageId::Source,
    StageId::Decoder,
    StageId::Processing,
    StageId::Output,
    StageId::OsLevel,
    StageId::Device,
];

/// Модель окна «Путь сигнала»: строится один раз на событие (§3.6, ТЗ-79, ТЗ-83).
#[derive(Clone, PartialEq, Debug)]
pub struct PathReport {
    pub subject_title: String,
    pub stages: [StageRow; 6],
    pub recommendations: SmallVec<[Recommendation; 6]>,
    /// ТЗ-78, во всех режимах.
    pub guarantee_boundary: &'static str,
    pub player_version: &'static str,
    pub os: String,
}

/// Строка этапа (§3.6, ТЗ-71). Заметки статус этапа не меняют (§6.25).
#[derive(Clone, PartialEq, Debug)]
pub struct StageRow {
    pub id: StageId,
    /// Значок + текст, не только цвет.
    pub status: StageStatus,
    /// «Что изменилось».
    pub changes: SmallVec<[String; 4]>,
    /// «Почему».
    pub causes: SmallVec<[String; 4]>,
    pub confidence: Confidence,
    /// Lossy-источник, MD5, underrun/xrun, ОВ-35, RT-приоритет, буфер устройства, «ограничено N».
    pub notes: SmallVec<[String; 4]>,
}

impl StageRow {
    fn new(id: StageId, status: StageStatus, confidence: Confidence) -> StageRow {
        StageRow { id, status, changes: SmallVec::new(), causes: SmallVec::new(), confidence, notes: SmallVec::new() }
    }

    /// Этап, который не построен или не открыт: статус `Unknown`, одна заметка.
    fn placeholder(id: StageId, note: &str) -> StageRow {
        let mut row = StageRow::new(id, StageStatus::Unknown, Confidence::Assumed);
        row.notes.push(note.to_owned());
        row
    }
}

/// Отчёт окна (§3.6, §6.25). Чистая функция из готовых данных (ТЗ-79):
/// - `Opened` — шесть этапов из `SignalPath::stages()` с заметками динамики;
/// - `Skipped` — этапы до `blocked.at` из частичного плана, этап `blocked.at`
///   «несовместимо» (статус `Lossy` с текстом причины, ТЗ-80), последующие —
///   «не построен»;
/// - `NoStream` — все этапы «поток не открыт» (ТЗ-56).
pub fn build_report(subject: PathSubject<'_>, dyn_state: &DynamicPathState, settings: &ModeSettings) -> PathReport {
    let (subject_title, stages) = match subject {
        PathSubject::Opened(path) => (opened_title(path), opened_rows(path, dyn_state)),
        PathSubject::Skipped(blocked) => (skipped_title(blocked), skipped_rows(blocked)),
        PathSubject::NoStream => {
            ("Поток не открыт".to_owned(), STAGE_ORDER.map(|id| StageRow::placeholder(id, "поток не открыт")))
        }
    };
    PathReport {
        subject_title,
        stages,
        recommendations: recommend(subject, dyn_state, settings),
        guarantee_boundary: GUARANTEE_BOUNDARY,
        player_version: env!("CARGO_PKG_VERSION"),
        os: format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
    }
}

// ---------------------------------------------------------------------------
// Открытый поток
// ---------------------------------------------------------------------------

fn opened_title(path: &SignalPath) -> String {
    format!("{} — {}", source_text(&path.source.fmt), mode_name(path.mode))
}

fn opened_rows(path: &SignalPath, d: &DynamicPathState) -> [StageRow; 6] {
    [
        source_row(&path.source),
        decoder_row(&path.source.fmt, &path.decoder, d),
        processing_row(&path.processing, d, driver_confidence(&path.output.driver)),
        output_row(&path.output, d),
        os_row(&path.os),
        device_row(&path.device),
    ]
}

/// «Источник» (ТЗ-74): формат из открытого файла; для lossy — пометка и
/// «24 бит (выход декодера)».
fn source_row(stage: &SourceStage) -> StageRow {
    let fmt = &stage.fmt;
    let mut row = StageRow::new(StageId::Source, StageStatus::Unchanged, Confidence::Confirmed(ConfirmedBy::FileHeader));
    row.notes.push(format!("{} (из открытого файла)", source_text(fmt)));
    if fmt.lossy {
        row.notes.push(format!("источник с потерями ({}): 24 бит (выход декодера)", codec_name(fmt.codec)));
    }
    row
}

/// «Декодер» (ТЗ-75): округление lossy, MD5, ошибки декодирования,
/// «ограничено N сэмплов» (ОВС-11). MD5 статус не меняет (ОВ-15).
fn decoder_row(fmt: &SourceFormat, stage: &DecoderStage, d: &DynamicPathState) -> StageRow {
    let status = if stage.lossless { StageStatus::Unchanged } else { StageStatus::Lossy };
    let mut row = StageRow::new(StageId::Decoder, status, Confidence::Confirmed(ConfirmedBy::FileHeader));
    if !stage.lossless {
        row.changes.push("с потерями, округление до 24 бит без дизеринга".to_owned());
        row.causes.push(format!("кодек с потерями ({})", codec_name(fmt.codec)));
    }
    if let Some(md5) = md5_text(d.md5) {
        row.notes.push(md5.to_owned());
    }
    if d.decode_errors > 0 {
        row.notes.push(format!("ошибок декодирования: {}", d.decode_errors));
    }
    if d.lossy_clipped > 0 {
        row.notes.push(format!("ограничено {} сэмплов", d.lossy_clipped));
    }
    row
}

/// «Обработка» (ТЗ-71, ТЗ-72): каждое преобразование с причиной; заметки
/// «ограничено N сэмплов» (ТЗ-129) и фактическая граница фильтра DSD→PCM,
/// если она меньше выбранной (ТЗ-16).
fn processing_row(stage: &ProcessingStage, d: &DynamicPathState, empty: Confidence) -> StageRow {
    let mut row = StageRow::new(StageId::Processing, stage.status(), transforms_confidence(&stage.transforms, empty));
    push_transforms(&mut row, &stage.transforms);
    if d.dsd_clipped > 0 {
        row.notes.push(format!("ограничено {} сэмплов", d.dsd_clipped));
    }
    row
}

fn push_transforms(row: &mut StageRow, transforms: &[Transform]) {
    for t in transforms {
        row.changes.push(t.kind.describe().to_string());
        row.causes.push(cause_text(&t.cause));
        if let TransformKind::DsdToPcm { filter, stop_hz, .. } = t.kind {
            let chosen = dsd_filter_hz(filter);
            if stop_hz < chosen {
                row.notes.push(format!("фактическая граница фильтра {} (выбрано {})", khz(stop_hz), khz(chosen)));
            }
        }
    }
}

/// «Вывод» (ТЗ-76): API, доступ, устройство, запрошенный и принятый формат,
/// ОВ-35, буфер устройства (ТЗ-131), RT-приоритет (ОВС-8), Underrun/Xrun (ТЗ-68).
fn output_row(stage: &OutputStage, d: &DynamicPathState) -> StageRow {
    let mut row = StageRow::new(StageId::Output, stage.status(), driver_confidence(&stage.driver));
    row.notes.push(format!("{}, «{}»", api_name(stage.api, stage.access), stage.device_name));
    row.notes.push(format!("запрошено: {}", stream_text(&stage.requested)));
    match stage.driver {
        DriverFormat::Confirmed(actual) => {
            if actual != stage.requested {
                row.changes.push(format!("драйвер принял {}", stream_text(&actual)));
                row.causes.push(cause_text(&Cause::DriverAccepted));
            }
            row.notes.push(format!("фактически: {} (прочитано у драйвера)", stream_text(&actual)));
        }
        DriverFormat::Unreadable { .. } => row.notes.push("запрошено, не подтверждено".to_owned()),
    }
    if stage.reservation_warning {
        row.notes.push("устройство открыто без резервирования: сервера резервирования нет".to_owned());
    }
    if let Some(buf) = stage.device_buffer {
        row.notes.push(device_buffer_text(buf, &stage.requested));
    }
    match d.rt_priority {
        RtPriorityState::Granted(prio) => row.notes.push(format!("приоритет реального времени: получен ({prio})")),
        RtPriorityState::Denied => row.notes.push("приоритет реального времени: не получен".to_owned()),
        RtPriorityState::NotRequested | RtPriorityState::Pending => {}
    }
    row.notes.push(format!("Underrun: {}", d.underruns));
    if stage.access == AccessKind::Exclusive {
        row.notes.push(format!("Xrun: {}", d.xruns));
    }
    row
}

/// «Уровень ОС» (ТЗ-77).
fn os_row(stage: &OsStage) -> StageRow {
    let confidence = match stage {
        OsStage::NotInvolved => Confidence::Confirmed(ConfirmedBy::DriverHwParams),
        OsStage::Known { confidence, .. } => *confidence,
        OsStage::Unknown => Confidence::Assumed,
    };
    let mut row = StageRow::new(StageId::OsLevel, stage.status(), confidence);
    let note = match stage {
        OsStage::NotInvolved => "не участвует: монопольный доступ к устройству".to_owned(),
        OsStage::Known { server, rate, format, .. } => {
            let rate = rate.map_or_else(|| "частота неизвестна".to_owned(), |r| format!("частота {}", khz(r.hz())));
            let format = format.as_deref().unwrap_or("формат неизвестен");
            format!("{}: {rate}, {format}; не контролируется плеером", server_name(*server))
        }
        OsStage::Unknown => "не контролируется плеером, возможно преобразование".to_owned(),
    };
    row.notes.push(note);
    row
}

/// «Устройство» (ТЗ-78): всегда `Unknown`, граница гарантии — `PathReport::guarantee_boundary`.
fn device_row(stage: &DeviceStage) -> StageRow {
    let mut row = StageRow::new(StageId::Device, stage.status(), Confidence::Assumed);
    let note = match stage.accepted {
        Some(f) => format!("принят формат {}", stream_text(&f)),
        None => "принятый формат не подтверждён".to_owned(),
    };
    row.notes.push(note);
    row
}

// ---------------------------------------------------------------------------
// Пропущенный трек (ТЗ-80)
// ---------------------------------------------------------------------------

fn skipped_title(blocked: &Blocked) -> String {
    format!(
        "{} — {}: трек пропущен",
        source_text(&blocked.partial.source),
        mode_name(blocked.partial.mode)
    )
}

fn skipped_rows(blocked: &Blocked) -> [StageRow; 6] {
    let plan = &blocked.partial;
    let mut reached = true;
    STAGE_ORDER.map(|id| {
        if !reached {
            return StageRow::placeholder(id, "не построен");
        }
        if id == blocked.at {
            reached = false;
            let mut row = StageRow::new(id, StageStatus::Lossy, Confidence::Assumed);
            row.changes.push("несовместимо".to_owned());
            row.causes.push(incompatibility_text(&blocked.reason));
            return row;
        }
        match id {
            StageId::Source => source_row(&SourceStage { fmt: plan.source.clone() }),
            StageId::Decoder => {
                decoder_row(&plan.source, &DecoderStage { lossless: !plan.source.lossy }, &DynamicPathState::default())
            }
            StageId::Processing => {
                let mut row = StageRow::new(
                    id,
                    ProcessingStage { transforms: plan.transforms.clone() }.status(),
                    transforms_confidence(&plan.transforms, Confidence::Assumed),
                );
                push_transforms(&mut row, &plan.transforms);
                row
            }
            StageId::Output | StageId::OsLevel | StageId::Device => StageRow::placeholder(id, "не построен"),
        }
    })
}

/// Текст причины несовместимости (ТЗ-80): «частота 192 кГц не поддерживается устройством».
fn incompatibility_text(reason: &Incompatibility) -> String {
    match reason {
        Incompatibility::RateUnsupported { rate, .. } => {
            format!("частота {} не поддерживается устройством", khz(rate.hz()))
        }
        Incompatibility::TruncationForbidden { from, max } => {
            format!("{} бит: устройство принимает не более {max} бит, усечение запрещено режимом", from.bits())
        }
        Incompatibility::ChannelsUnsupported { src, .. } => {
            format!("{src} кан. не поддерживается устройством")
        }
        Incompatibility::DopUnsupported { dsd, max_dop } => match max_dop {
            Some(max) => format!("DoP {} не поддерживается устройством (не выше {})", dsd_name(*dsd), dsd_name(*max)),
            None => format!("DoP {} не поддерживается устройством", dsd_name(*dsd)),
        },
        Incompatibility::DriverRejectedRate { requested, actual } => format!(
            "драйвер принял {} вместо запрошенной {}",
            khz(actual.hz()),
            khz(requested.hz())
        ),
        Incompatibility::DriverRejectedFormat { requested, actual } => format!(
            "драйвер принял формат {} вместо {}",
            sample_name(*actual),
            sample_name(*requested)
        ),
        Incompatibility::FloatUnsupported => "PCM с плавающей точкой не воспроизводится в Строгом режиме".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Достоверность и тексты
// ---------------------------------------------------------------------------

fn driver_confidence(driver: &DriverFormat) -> Confidence {
    match driver {
        DriverFormat::Confirmed(_) => Confidence::Confirmed(ConfirmedBy::DriverHwParams),
        DriverFormat::Unreadable { .. } => Confidence::Assumed,
    }
}

/// Достоверность этапа «Обработка»: `Assumed`, если хоть одно преобразование
/// предположено; иначе подтверждение первого; без преобразований — `empty`.
fn transforms_confidence(transforms: &[Transform], empty: Confidence) -> Confidence {
    if transforms.iter().any(|t| t.confidence == Confidence::Assumed) {
        return Confidence::Assumed;
    }
    transforms.first().map_or(empty, |t| t.confidence)
}

fn cause_text(cause: &Cause) -> String {
    match cause {
        Cause::PlayerSetting(param) => {
            let label = PARAMS.iter().find(|d| d.id == *param).map_or("—", |d| d.label_key);
            format!("настройка плеера: {label}")
        }
        Cause::DeviceLimit => "ограничение устройства".to_owned(),
        Cause::DriverAccepted => "драйвер принял другой формат".to_owned(),
        Cause::ServerLimit => "аудиосервер не принимает формат".to_owned(),
        Cause::ModeRule => "правило режима".to_owned(),
        Cause::FormatContainer => "формат контейнера устройства".to_owned(),
    }
}

fn md5_text(md5: Md5Status) -> Option<&'static str> {
    match md5 {
        Md5Status::Pending => Some("MD5: проверка после окончания трека"),
        Md5Status::Matched => Some("MD5 совпал"),
        Md5Status::Mismatched => Some("MD5 не совпал"),
        Md5Status::Absent => Some("MD5 в файле нет"),
        Md5Status::NotChecked(NotCheckedWhy::Seeked) => Some("MD5 не проверен: была перемотка"),
        Md5Status::NotChecked(NotCheckedWhy::NotFlac) => None,
    }
}

/// «буфер устройства 42 мс (период 512 кадров)»; миллисекунды — целочисленно по частоте потока.
fn device_buffer_text(buf: DeviceBufferActual, fmt: &StreamFormat) -> String {
    let ms = u64::from(buf.buffer_frames) * 1000 / u64::from(fmt.rate.hz());
    format!("буфер устройства {ms} мс (период {} кадров)", buf.period_frames)
}

fn source_text(fmt: &SourceFormat) -> String {
    let depth = match fmt.kind {
        SourceKind::Pcm { bits } => format!("{} бит", bits.bits()),
        SourceKind::FloatPcm { double: false } => "float32".to_owned(),
        SourceKind::FloatPcm { double: true } => "float64".to_owned(),
        SourceKind::Dsd { dsd } => dsd_name(dsd).to_owned(),
    };
    format!("{} {depth} / {}, {} кан.", codec_name(fmt.codec), khz(fmt.rate.hz()), fmt.layout.count().get())
}

fn stream_text(fmt: &StreamFormat) -> String {
    format!("{}, {}, {} кан.", khz(fmt.rate.hz()), sample_name(fmt.sample), fmt.channels.get())
}

fn sample_name(s: OutputSampleFormat) -> &'static str {
    match s {
        OutputSampleFormat::S16Le => "16 бит",
        OutputSampleFormat::S24_3Le => "24 бит (3 байта)",
        OutputSampleFormat::S24Le => "24 бит (в 32)",
        OutputSampleFormat::S32Le => "32 бит",
        OutputSampleFormat::F32 => "float32",
    }
}

fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::Flac => "FLAC",
        Codec::Alac => "ALAC",
        Codec::Wav => "WAV",
        Codec::Aiff => "AIFF",
        Codec::Mp3 => "MP3",
        Codec::Aac => "AAC",
        Codec::Vorbis => "Vorbis",
        Codec::Adpcm => "ADPCM",
        Codec::Dsf => "DSF",
        Codec::Dff => "DFF",
        Codec::Other => "?",
    }
}

fn mode_name(mode: ModeKind) -> &'static str {
    match mode {
        ModeKind::Compatible => "Совместимый режим",
        ModeKind::Optimal => "Оптимальный режим",
        ModeKind::Strict => "Строгий режим",
    }
}

fn api_name(api: OutputApi, access: AccessKind) -> &'static str {
    match (api, access) {
        (OutputApi::AlsaHw, _) => "ALSA hw, Exclusive",
        (OutputApi::PipeWire, _) => "PipeWire, Shared",
        (OutputApi::PulseAudio, _) => "PulseAudio, Shared",
        (OutputApi::WasapiShared, _) => "WASAPI, Shared",
        (OutputApi::CoreAudio, AccessKind::Exclusive) => "Core Audio, Exclusive",
        (OutputApi::CoreAudio, AccessKind::Shared) => "Core Audio, Shared",
    }
}

fn server_name(server: OsServer) -> &'static str {
    match server {
        OsServer::WasapiMixer => "микшер WASAPI",
        OsServer::PipeWire => "PipeWire",
        OsServer::PulseAudio => "PulseAudio",
    }
}

/// Выбранная граница фильтра DSD→PCM в Гц (ОВС-6, ТЗ-128).
fn dsd_filter_hz(filter: DsdFilter) -> u32 {
    match filter {
        DsdFilter::K24 => 24_000,
        DsdFilter::K30 => 30_000,
        DsdFilter::K50 => 50_000,
    }
}
