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
use crate::settings::params::{param_label, ParamId};
use crate::settings::playback::{DsdFilter, ModeKind, ModeSettings};

use super::recommend::{
    recommend, HwRequirement, OsTopic, PathSubject, PlayerSettingRec, RecAction, Recommendation, SwitchWhy,
};
use super::signal::{DynamicPathState, Md5Status, NotCheckedWhy, RtPriorityState, SignalPath};
use super::transform::{dsd_name, khz, Cause, Confidence, ConfirmedBy, Transform, TransformKind};
use super::{
    Blocked, DecoderStage, DeviceStage, OsServer, OsStage, OutputStage, ProcessingStage, Route, SourceStage, StageId,
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
        device_row(&path.device, &path.route),
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
/// если она меньше выбранной (ТЗ-16). Достоверность — от драйвера: формат,
/// прочитанный у драйвера, подтверждает выход цепочки преобразований.
fn processing_row(stage: &ProcessingStage, d: &DynamicPathState, driver: Confidence) -> StageRow {
    let confidence = match driver {
        Confidence::Confirmed(_) => driver,
        Confidence::Assumed => transforms_confidence(&stage.transforms, Confidence::Assumed),
    };
    let mut row = StageRow::new(StageId::Processing, stage.status(), confidence);
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
/// Для DoP-потока — подсказка про тишину на ЦАП без декодера DoP (ТЗ-145,
/// ADR-25, §6.25): заметка статус этапа не меняет.
fn device_row(stage: &DeviceStage, route: &Route) -> StageRow {
    let mut row = StageRow::new(StageId::Device, stage.status(), Confidence::Assumed);
    let note = match stage.accepted {
        Some(f) => format!("принят формат {}", stream_text(&f)),
        None => "принятый формат не подтверждён".to_owned(),
    };
    row.notes.push(note);
    if matches!(route, Route::Dop(_)) {
        row.notes.push(
            "если ЦАП молчит — он не понимает DoP: снимите «ЦАП понимает DoP»".to_owned(),
        );
    }
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

/// Текст несовместимости для сообщения о пропуске трека: этап и причина
/// теми же словами, что в окне «Путь сигнала» (ТЗ-57, ТЗ-80).
pub fn blocked_text(blocked: &Blocked) -> String {
    format!("{}: {}", stage_name(blocked.at), incompatibility_text(&blocked.reason))
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
        Incompatibility::DopDisabled { device } => {
            format!("DoP выключен для устройства {}", device.as_str())
        }
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

/// Текст причины (ТЗ-81, ТЗ-95, §6.25): название параметра — русский
/// ярлык (`param_label`), не внутренний `label_key`.
fn cause_text(cause: &Cause) -> String {
    match cause {
        // ТЗ-145, ADR-25: текст точнее, чем «настройка плеера: <ярлык>».
        Cause::PlayerSetting(ParamId::DeviceDop) => "DoP выключен для устройства".to_owned(),
        Cause::PlayerSetting(param) => format!("настройка плеера: {}", param_label(*param)),
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

// ---------------------------------------------------------------------------
// Display (ТЗ-84, §6.25)
// ---------------------------------------------------------------------------

/// Текстовое представление отчёта (ТЗ-84, §6.25): заголовок, шесть блоков
/// «Этап: статус / изменения / причины / достоверность / заметки»,
/// рекомендации по трём группам («Настройки плеера», «Настройки ОС»,
/// «Оборудование» — смена режима показывается строкой внутри этой группы,
/// так как сопровождает её рекомендации), версия плеера, ОС и ядро.
/// Без разметки UI.
impl std::fmt::Display for PathReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.subject_title)?;
        for row in &self.stages {
            writeln!(f)?;
            writeln!(f, "{}: {}", stage_name(row.id), status_text(row.status))?;
            if !row.changes.is_empty() {
                writeln!(f, "  изменения: {}", row.changes.join("; "))?;
            }
            if !row.causes.is_empty() {
                writeln!(f, "  причины: {}", row.causes.join("; "))?;
            }
            writeln!(f, "  достоверность: {}", confidence_text(row.confidence))?;
            if !row.notes.is_empty() {
                writeln!(f, "  заметки: {}", row.notes.join("; "))?;
            }
        }

        let (player, os, hardware) = group_recommendations(&self.recommendations);
        writeln!(f)?;
        writeln!(f, "Рекомендации:")?;
        if player.is_empty() && os.is_empty() && hardware.is_empty() {
            writeln!(f, "  нет")?;
        } else {
            write_group(f, "Настройки плеера", &player)?;
            write_group(f, "Настройки ОС", &os)?;
            write_group(f, "Оборудование", &hardware)?;
        }

        writeln!(f)?;
        writeln!(f, "{}", self.guarantee_boundary)?;
        writeln!(f)?;
        writeln!(f, "Версия плеера: {}", self.player_version)?;
        write!(f, "ОС: {}", self.os)
    }
}

fn write_group(f: &mut std::fmt::Formatter<'_>, title: &str, items: &[String]) -> std::fmt::Result {
    if items.is_empty() {
        return Ok(());
    }
    writeln!(f, "  {title}:")?;
    for item in items {
        writeln!(f, "    - {item}")?;
    }
    Ok(())
}

/// Три группы рекомендаций (ТЗ-81): «Настройки плеера», «Настройки ОС»,
/// «Оборудование» — смена режима попадает в `Hardware`, как поясняет
/// комментарий группировки ниже.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecGroup {
    Player,
    Os,
    Hardware,
}

/// Группа рекомендации (ТЗ-81): `SwitchMode` — в `Hardware` (§6.25, см.
/// `group_recommendations`).
pub fn recommendation_group(rec: &Recommendation) -> RecGroup {
    match rec {
        Recommendation::PlayerSetting(_) => RecGroup::Player,
        Recommendation::Os { .. } => RecGroup::Os,
        Recommendation::Hardware { .. } | Recommendation::SwitchMode { .. } => RecGroup::Hardware,
    }
}

/// Текст рекомендации (ТЗ-81, ТЗ-84): та же строка, что в `Display` окна
/// «Путь сигнала», для UI-презентера без дублирования текста.
pub fn recommendation_text(rec: &Recommendation) -> String {
    match rec {
        Recommendation::PlayerSetting(r) => player_setting_text(r),
        Recommendation::Os { topic } => os_topic_text(*topic).to_owned(),
        Recommendation::Hardware { need } => hw_requirement_text(need),
        Recommendation::SwitchMode { to, why } => {
            format!("смена режима: {} ({})", mode_name(*to), switch_why_text(*why))
        }
    }
}

/// Группировка рекомендаций по трём группам (§6.25): «Настройки плеера»,
/// «Настройки ОС», «Оборудование» (смена режима — строкой в группе
/// «Оборудование», так как в таблице §6.25 она всегда сопровождает
/// рекомендацию по оборудованию или несовместимости режима).
fn group_recommendations(recs: &[Recommendation]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut player = Vec::new();
    let mut os = Vec::new();
    let mut hardware = Vec::new();
    for rec in recs {
        let text = recommendation_text(rec);
        match recommendation_group(rec) {
            RecGroup::Player => player.push(text),
            RecGroup::Os => os.push(text),
            RecGroup::Hardware => hardware.push(text),
        }
    }
    (player, os, hardware)
}

/// Название этапа (ТЗ-71, ТЗ-84): для окна «Путь сигнала» и UI-презентера.
pub fn stage_name(id: StageId) -> &'static str {
    match id {
        StageId::Source => "Источник",
        StageId::Decoder => "Декодер",
        StageId::Processing => "Обработка",
        StageId::Output => "Вывод",
        StageId::OsLevel => "Уровень ОС",
        StageId::Device => "Устройство",
    }
}

/// Текст статуса этапа (ТЗ-71, ТЗ-84): значок + текст, не только цвет.
pub fn status_text(status: StageStatus) -> &'static str {
    match status {
        StageStatus::Unchanged => "✓ без изменений",
        StageStatus::Lossless => "≈ изменено без потерь",
        StageStatus::Unknown => "? неизвестно",
        StageStatus::Lossy => "✗ с потерями / несовместимо",
    }
}

/// Текст достоверности этапа (ТЗ-71, ТЗ-84): «подтверждено (…)» / «предположено».
pub fn confidence_text(c: Confidence) -> String {
    match c {
        Confidence::Confirmed(by) => format!("подтверждено ({})", confirmed_by_text(by)),
        Confidence::Assumed => "предположено".to_owned(),
    }
}

fn confirmed_by_text(by: ConfirmedBy) -> &'static str {
    match by {
        ConfirmedBy::DriverHwParams => "hw_params драйвера",
        ConfirmedBy::RuntimeTest => "тест тракта",
        ConfirmedBy::FileHeader => "заголовок файла",
        ConfirmedBy::OsQuery => "запрос к ОС",
    }
}

/// Текст рекомендации «Настройки плеера» (ТЗ-81, ТЗ-95, §6.25): русский
/// ярлык параметра (`param_label`), не внутренний `label_key`.
fn player_setting_text(rec: &PlayerSettingRec) -> String {
    format!("{}: {}", param_label(rec.param()), rec_action_text(rec.action()))
}

fn rec_action_text(a: RecAction) -> &'static str {
    match a {
        RecAction::SetVolumeFull => "громкость → 100 %",
        RecAction::Unmute => "выключить mute",
        RecAction::TurnOff => "выключить",
        RecAction::TurnOn => "включить",
        RecAction::ChooseQualityFilter => "выбрать фильтр SRC «Качество»",
        RecAction::Increase => "увеличить",
    }
}

fn os_topic_text(t: OsTopic) -> &'static str {
    match t {
        OsTopic::PipeWireAllowedRates => "PipeWire: allowed-rates",
        OsTopic::PipeWireForceRate => "PipeWire: force-rate",
        OsTopic::WindowsSharedFormat => "формат общего режима WASAPI",
        OsTopic::WindowsExclusivePermission => "разрешение на монопольный доступ WASAPI",
        OsTopic::RealtimePriority => "приоритет реального времени (rtkit, группа audio)",
    }
}

/// Текст совета «Оборудование» (ТЗ-80, ТЗ-81, §6.25): «ЦАП с поддержкой …»;
/// для DoP — с частотой PCM-контейнера.
fn hw_requirement_text(need: &HwRequirement) -> String {
    match need {
        HwRequirement::Rate(r) => format!("ЦАП с поддержкой {}", khz(r.hz())),
        HwRequirement::BitDepth(b) => format!("ЦАП с поддержкой {} бит", b.bits()),
        HwRequirement::Channels(n) => format!("ЦАП с поддержкой {n} каналов"),
        HwRequirement::DopRate(d) => {
            format!("ЦАП с поддержкой DoP {} (PCM {})", dsd_name(*d), khz(d.bit_rate_hz() / 16))
        }
    }
}

fn switch_why_text(why: SwitchWhy) -> &'static str {
    match why {
        SwitchWhy::Resample => "ресемплинг",
        SwitchWhy::Truncate => "усечение разрядности",
        SwitchWhy::Downmix => "сведение каналов",
        SwitchWhy::DsdToPcm => "DSD→PCM",
        SwitchWhy::Exclusive => "Exclusive",
    }
}

#[cfg(test)]
mod tests {
    //! Тесты окна «Путь сигнала», ч. 1 (§7.2, ТЗ-70–ТЗ-75).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

    use smallvec::SmallVec;

    use super::*;
    use crate::audio::backend::{DeviceCaps, HwDeviceId, RateSet};
    use crate::audio::format::{BitDepth, ChannelLayout, ChannelPos, Container, DsdRate, SampleRate};
    use crate::audio::path::plan::{plan, plan_with_device};
    use crate::audio::path::{OpenedFacts, PathPlan, ReservationView};
    use crate::settings::playback::{DeviceOpts, PathPolicy};

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn flac(bits: u8, rate: u32) -> SourceFormat {
        SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: sr(rate),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: BitDepth::new(bits).unwrap() },
        }
    }

    fn mp3(rate: u32) -> SourceFormat {
        SourceFormat { container: Container::Mp3, codec: Codec::Mp3, lossy: true, ..flac(24, rate) }
    }

    fn src_dsd(d: DsdRate) -> SourceFormat {
        SourceFormat {
            container: Container::Dsf,
            codec: Codec::Dsf,
            lossy: false,
            rate: sr(d.bit_rate_hz()),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Dsd { dsd: d },
        }
    }

    fn hw_device_id() -> HwDeviceId {
        let Ok(id) = "hw:CARD=A,DEV=0".parse::<HwDeviceId>() else {
            panic!("valid HwDeviceId");
        };
        id
    }

    fn five_one(mut s: SourceFormat) -> SourceFormat {
        use ChannelPos::{BL, BR, FC, FL, FR, LFE};
        s.layout = ChannelLayout::new(&[FL, FR, FC, LFE, BL, BR]).unwrap();
        s
    }

    fn exclusive(rates: &[u32], formats: &[OutputSampleFormat]) -> DeviceCaps {
        DeviceCaps {
            api: OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            name: "test".to_owned(),
            rates: RateSet::Discrete(rates.iter().map(|&h| sr(h)).collect()),
            formats: SmallVec::from_slice(formats),
            channels: SmallVec::from_slice(&[2]),
        }
    }

    fn opened(p: PathPlan) -> SignalPath {
        let facts = OpenedFacts {
            access: AccessKind::Exclusive,
            api: OutputApi::AlsaHw,
            device_name: "test".to_owned(),
            driver: DriverFormat::Confirmed(p.target),
            reservation: ReservationView::NotApplicable,
            os: OsStage::NotInvolved,
            device_buffer: None,
        };
        SignalPath::from_opened(p, facts, 1)
    }

    /// Отчёт Оптимального режима для `src` на Exclusive-устройстве.
    fn optimal_report(src: &SourceFormat, caps: &DeviceCaps, d: &DynamicPathState) -> PathReport {
        let s = ModeSettings::default();
        let path = opened(plan(src, caps, PathPolicy::Optimal(&s.optimal)).unwrap());
        build_report(PathSubject::Opened(&path), d, &s)
    }

    fn row(r: &PathReport, id: StageId) -> &StageRow {
        r.stages.iter().find(|s| s.id == id).unwrap()
    }

    fn has(list: &[String], needle: &str) -> bool {
        list.iter().any(|s| s.contains(needle))
    }

    #[test]
    fn report_has_six_stages_in_order() {
        let s = ModeSettings::default();
        let caps = exclusive(&[44_100], &[OutputSampleFormat::S32Le]);
        let reports = [
            optimal_report(&flac(16, 44_100), &caps, &DynamicPathState::default()),
            build_report(PathSubject::NoStream, &DynamicPathState::default(), &s),
        ];
        for r in &reports {
            let ids: Vec<StageId> = r.stages.iter().map(|s| s.id).collect();
            assert_eq!(ids, STAGE_ORDER.to_vec());
            assert_eq!(r.guarantee_boundary, GUARANTEE_BOUNDARY);
        }
    }

    #[test]
    fn report_stage_fields_optimal_192_on_96() {
        let caps = exclusive(&[96_000], &[OutputSampleFormat::S32Le]);
        let r = optimal_report(&flac(24, 192_000), &caps, &DynamicPathState::default());
        let p = row(&r, StageId::Processing);
        assert_eq!(p.status, StageStatus::Lossy);
        assert!(has(&p.changes, "192 кГц → 96 кГц"), "{:?}", p.changes);
        assert!(has(&p.causes, "ограничение устройства"), "{:?}", p.causes);
        assert!(matches!(p.confidence, Confidence::Confirmed(_)), "{:?}", p.confidence);
    }

    #[test]
    fn report_lists_all_transforms() {
        let caps = exclusive(&[96_000], &[OutputSampleFormat::S16Le]);
        let s = ModeSettings::default();
        let src = five_one(flac(24, 192_000));
        let path = opened(plan(&src, &caps, PathPolicy::Optimal(&s.optimal)).unwrap());
        let r = build_report(PathSubject::Opened(&path), &DynamicPathState::default(), &s);
        let p = row(&r, StageId::Processing);
        let ts = &path.processing.transforms;
        assert!(ts.len() >= 2, "{:?}", p.changes);
        assert_eq!(p.changes.len(), ts.len());
        assert_eq!(p.causes.len(), ts.len(), "каждое преобразование со своей причиной");
        for (change, t) in p.changes.iter().zip(ts.iter()) {
            assert_eq!(*change, t.kind.describe().to_string());
        }
        assert!(has(&p.changes, "SRC"), "{:?}", p.changes);
        assert!(has(&p.changes, "downmix"), "{:?}", p.changes);
        // «усечение 24 → 16» план пока не строит — AM1.0-B7.
    }

    #[test]
    fn report_source_stage() {
        let caps = exclusive(&[44_100, 96_000], &[OutputSampleFormat::S32Le]);
        let r = optimal_report(&mp3(44_100), &caps, &DynamicPathState::default());
        let src = row(&r, StageId::Source);
        assert!(has(&src.notes, "с потерями"), "{:?}", src.notes);
        assert!(has(&src.notes, "24 бит (выход декодера)"), "{:?}", src.notes);

        let r = optimal_report(&five_one(flac(24, 96_000)), &caps, &DynamicPathState::default());
        let src = row(&r, StageId::Source);
        assert!(has(&src.notes, "FLAC 24 бит / 96 кГц, 6 кан. (из открытого файла)"), "{:?}", src.notes);
        assert!(!has(&src.notes, "с потерями"));
        assert_eq!(src.confidence, Confidence::Confirmed(ConfirmedBy::FileHeader));
    }

    #[test]
    fn report_decoder_stage() {
        let caps = exclusive(&[44_100], &[OutputSampleFormat::S24Le]);
        let d = DynamicPathState { decode_errors: 1, ..DynamicPathState::default() };
        let r = optimal_report(&flac(16, 44_100), &caps, &d);
        assert!(has(&row(&r, StageId::Decoder).notes, "ошибок декодирования: 1"));

        let r = optimal_report(&mp3(44_100), &caps, &DynamicPathState::default());
        let dec = row(&r, StageId::Decoder);
        assert_eq!(dec.status, StageStatus::Lossy);
        assert!(has(&dec.changes, "с потерями, округление до 24 бит"), "{:?}", dec.changes);
        assert!(row(&r, StageId::Processing).changes.is_empty(), "{:?}", row(&r, StageId::Processing).changes);
    }

    /// §7.2 `report_output_stage_driver_facts` (ТЗ-76): факты этапа «Вывод» —
    /// частота у драйвера, непрочитанные параметры, Shared без «Exclusive»,
    /// предупреждение ОВ-35, буфер устройства, RT-приоритет. Заметки статус
    /// этапа не меняют.
    #[test]
    fn report_output_stage_driver_facts() {
        let s = ModeSettings::default();
        let caps = exclusive(&[48_000], &[OutputSampleFormat::S32Le]);
        let plan_ok = plan(&flac(24, 48_000), &caps, PathPolicy::Optimal(&s.optimal)).unwrap();
        let requested = plan_ok.target;
        let actual = StreamFormat { sample: OutputSampleFormat::S24Le, ..requested };

        // Драйвер принял другой формат: фактическая частота/формат «прочитано у
        // драйвера», предупреждение ОВ-35, буфер устройства, RT-приоритет получен.
        let facts = OpenedFacts {
            access: AccessKind::Exclusive,
            api: OutputApi::AlsaHw,
            device_name: "test".to_owned(),
            driver: DriverFormat::Confirmed(actual),
            reservation: ReservationView::UnavailableNoServer,
            os: OsStage::NotInvolved,
            device_buffer: Some(DeviceBufferActual { period_frames: 256, buffer_frames: 2016 }),
        };
        let d = DynamicPathState { rt_priority: RtPriorityState::Granted(10), ..DynamicPathState::default() };
        let path = SignalPath::from_opened(plan_ok.clone(), facts, 1);
        let r = build_report(PathSubject::Opened(&path), &d, &s);
        let out = row(&r, StageId::Output);
        assert_eq!(out.status, StageStatus::Unchanged);
        assert!(has(&out.notes, "прочитано у драйвера"), "{:?}", out.notes);
        assert!(has(&out.notes, "устройство открыто без резервирования"), "{:?}", out.notes);
        assert!(has(&out.notes, "буфер устройства 42 мс"), "{:?}", out.notes);
        assert!(has(&out.notes, "приоритет реального времени: получен"), "{:?}", out.notes);
        assert!(has(&out.notes, "Underrun: 0"), "{:?}", out.notes);
        assert!(has(&out.notes, "Xrun: 0"), "{:?}", out.notes);

        // Драйвер не прочитан: «запрошено, не подтверждено».
        let facts_unreadable = OpenedFacts {
            access: AccessKind::Exclusive,
            api: OutputApi::AlsaHw,
            device_name: "test".to_owned(),
            driver: DriverFormat::Unreadable { requested },
            reservation: ReservationView::NotApplicable,
            os: OsStage::NotInvolved,
            device_buffer: None,
        };
        let path_unreadable = SignalPath::from_opened(plan_ok.clone(), facts_unreadable, 1);
        let r_unreadable = build_report(PathSubject::Opened(&path_unreadable), &DynamicPathState::default(), &s);
        assert!(has(&row(&r_unreadable, StageId::Output).notes, "запрошено, не подтверждено"));

        // Shared-доступ: нет строки про «Exclusive» и нет «Xrun» (только Exclusive).
        let facts_shared = OpenedFacts {
            access: AccessKind::Shared,
            api: OutputApi::PipeWire,
            device_name: "test".to_owned(),
            driver: DriverFormat::Confirmed(requested),
            reservation: ReservationView::NotApplicable,
            os: OsStage::NotInvolved,
            device_buffer: None,
        };
        let d_denied = DynamicPathState { rt_priority: RtPriorityState::Denied, ..DynamicPathState::default() };
        let path_shared = SignalPath::from_opened(plan_ok, facts_shared, 1);
        let r_shared = build_report(PathSubject::Opened(&path_shared), &d_denied, &s);
        let out_shared = row(&r_shared, StageId::Output);
        assert!(!has(&out_shared.notes, "Exclusive"), "{:?}", out_shared.notes);
        assert!(!has(&out_shared.notes, "Xrun"), "{:?}", out_shared.notes);
        assert!(has(&out_shared.notes, "приоритет реального времени: не получен"), "{:?}", out_shared.notes);
        assert_eq!(out_shared.status, StageStatus::Unchanged);
    }

    /// §7.2 `report_clip_notes` (ТЗ-75, ТЗ-129, ОВС-11): «ограничено N сэмплов»
    /// у «Декодер» (`lossy_clipped`) и «Обработка» (`dsd_clipped`); статус
    /// этапа не меняется.
    #[test]
    fn report_clip_notes() {
        let caps = exclusive(&[96_000], &[OutputSampleFormat::S32Le]);
        let zero = DynamicPathState::default();
        let clipped = DynamicPathState { lossy_clipped: 5, dsd_clipped: 3, ..DynamicPathState::default() };

        let r0 = optimal_report(&mp3(44_100), &caps, &zero);
        let r1 = optimal_report(&mp3(44_100), &caps, &clipped);
        assert!(has(&row(&r1, StageId::Decoder).notes, "ограничено 5 сэмплов"), "{:?}", row(&r1, StageId::Decoder).notes);
        assert_eq!(row(&r0, StageId::Decoder).status, row(&r1, StageId::Decoder).status);

        let r2 = optimal_report(&flac(24, 192_000), &caps, &clipped);
        assert!(
            has(&row(&r2, StageId::Processing).notes, "ограничено 3 сэмплов"),
            "{:?}",
            row(&r2, StageId::Processing).notes
        );
        let r2_zero = optimal_report(&flac(24, 192_000), &caps, &zero);
        assert_eq!(row(&r2, StageId::Processing).status, row(&r2_zero, StageId::Processing).status);
    }

    /// §7.2 `report_built_without_backend_access` (ТЗ-79): `build_report`
    /// строится только из конструкторов `PathPlan`/`Blocked`/`PathSubject` —
    /// без `Player`, `AudioHost` или иного обращения к движку/бэкенду.
    #[test]
    fn report_built_without_backend_access() {
        let s = ModeSettings::default();
        let caps = exclusive(&[44_100], &[OutputSampleFormat::S32Le]);
        let path = opened(plan(&flac(16, 44_100), &caps, PathPolicy::Optimal(&s.optimal)).unwrap());
        let opened_report = build_report(PathSubject::Opened(&path), &DynamicPathState::default(), &s);

        let dev96 = exclusive(&[96_000], &[OutputSampleFormat::S32Le]);
        let blocked = plan(&flac(24, 192_000), &dev96, PathPolicy::Strict(&s.strict)).unwrap_err();
        let skipped_report = build_report(PathSubject::Skipped(&blocked), &DynamicPathState::default(), &s);

        let no_stream_report = build_report(PathSubject::NoStream, &DynamicPathState::default(), &s);

        for r in [&opened_report, &skipped_report, &no_stream_report] {
            assert_eq!(r.stages.len(), 6);
        }
    }

    /// §7.2 `report_skipped_track_blocked_stage` (ТЗ-80): 24 бит/192 кГц на
    /// Exclusive-устройстве с потолком 96 кГц в Строгом режиме — этап
    /// `blocked.at` с причиной несовместимости и рекомендациями «Оборудование»
    /// + смена режима на Оптимальный.
    #[test]
    fn report_skipped_track_blocked_stage() {
        let s = ModeSettings::default();
        let dev96 = exclusive(&[44_100, 48_000, 96_000], &[OutputSampleFormat::S32Le]);
        let blocked = plan(&flac(24, 192_000), &dev96, PathPolicy::Strict(&s.strict)).unwrap_err();
        let r = build_report(PathSubject::Skipped(&blocked), &DynamicPathState::default(), &s);
        let stage = row(&r, blocked.at);
        assert!(has(&stage.causes, "частота 192 кГц не поддерживается устройством"), "{:?}", stage.causes);

        assert!(
            r.recommendations.contains(&Recommendation::SwitchMode { to: ModeKind::Optimal, why: SwitchWhy::Resample }),
            "{:?}",
            r.recommendations
        );
        assert!(
            r.recommendations.contains(&Recommendation::Hardware { need: HwRequirement::Rate(sr(192_000)) }),
            "{:?}",
            r.recommendations
        );
    }

    /// §7.2 `report_text_contains_all_parts` (ТЗ-84): `Display` содержит все
    /// шесть названий этапов, текст рекомендаций, версию плеера и ОС.
    #[test]
    fn report_text_contains_all_parts() {
        let s = ModeSettings::default();
        let dev96 = exclusive(&[44_100, 48_000, 96_000], &[OutputSampleFormat::S32Le]);
        let blocked = plan(&flac(24, 192_000), &dev96, PathPolicy::Strict(&s.strict)).unwrap_err();
        let r = build_report(PathSubject::Skipped(&blocked), &DynamicPathState::default(), &s);
        let text = format!("{r}");
        for name in ["Источник", "Декодер", "Обработка", "Вывод", "Уровень ОС", "Устройство"] {
            assert!(text.contains(name), "{name} отсутствует в:\n{text}");
        }
        assert!(text.contains("Оборудование"), "{text}");
        assert!(text.contains("смена режима"), "{text}");
        assert!(text.contains(r.player_version), "{text}");
        assert!(text.contains(&r.os), "{text}");
    }

    /// §7.2 `recommendation_text_matches_display_groups` (ТЗ-81, ТЗ-84):
    /// `recommendation_text` каждой рекомендации отчёта — те же строки,
    /// что в `Display`, без дублирования текста.
    #[test]
    fn recommendation_text_matches_display_groups() {
        let s = ModeSettings::default();
        let dev96 = exclusive(&[44_100, 48_000, 96_000], &[OutputSampleFormat::S32Le]);
        let blocked = plan(&flac(24, 192_000), &dev96, PathPolicy::Strict(&s.strict)).unwrap_err();
        let r = build_report(PathSubject::Skipped(&blocked), &DynamicPathState::default(), &s);
        let text = format!("{r}");
        for rec in &r.recommendations {
            assert!(text.contains(&recommendation_text(rec)), "{:?} отсутствует в:\n{text}", rec);
        }
    }

    /// §7.2 `recommendation_group_switch_mode_is_hardware` (ТЗ-81): смена
    /// режима группируется с «Оборудование», как в `group_recommendations`.
    #[test]
    fn recommendation_group_switch_mode_is_hardware() {
        let rec = Recommendation::SwitchMode { to: ModeKind::Optimal, why: SwitchWhy::Resample };
        assert_eq!(recommendation_group(&rec), RecGroup::Hardware);
    }

    /// §6.25, ТЗ-145, ADR-25: причина `DsdToPcm` в Оптимальном из-за
    /// `dev.dop = false` — «DoP выключен для устройства», а не общий текст
    /// «настройка плеера: <ярлык>».
    #[test]
    fn report_processing_cause_device_dop_off() {
        let s = ModeSettings::default();
        let id = hw_device_id();
        let caps = exclusive(&[44_100, 88_200, 176_400], &[OutputSampleFormat::S24Le, OutputSampleFormat::S32Le]);
        let p = plan_with_device(
            &src_dsd(DsdRate::Dsd64),
            &caps,
            PathPolicy::Optimal(&s.optimal),
            Some((&id, DeviceOpts { dop: false })),
        )
        .unwrap();
        let path = opened(p);
        let r = build_report(PathSubject::Opened(&path), &DynamicPathState::default(), &s);
        let proc = row(&r, StageId::Processing);
        assert!(has(&proc.causes, "DoP выключен для устройства"), "{:?}", proc.causes);
    }

    /// §6.25, ТЗ-145, ADR-25: заметка этапа «Устройство» про тишину на ЦАП
    /// без декодера DoP — только для DoP-потока, не для PCM.
    #[test]
    fn report_device_stage_dop_note() {
        let s = ModeSettings::default();
        let dop_caps = exclusive(&[176_400], &[OutputSampleFormat::S24Le]);
        let path = opened(plan(&src_dsd(DsdRate::Dsd64), &dop_caps, PathPolicy::Optimal(&s.optimal)).unwrap());
        let r = build_report(PathSubject::Opened(&path), &DynamicPathState::default(), &s);
        let dev = row(&r, StageId::Device);
        assert!(has(&dev.notes, "если ЦАП молчит"), "{:?}", dev.notes);

        let pcm_caps = exclusive(&[44_100], &[OutputSampleFormat::S32Le]);
        let r = optimal_report(&flac(16, 44_100), &pcm_caps, &DynamicPathState::default());
        let dev = row(&r, StageId::Device);
        assert!(!has(&dev.notes, "если ЦАП молчит"), "{:?}", dev.notes);
    }

    /// AM1.0-B27 (ТЗ-81, ТЗ-95, §6.25): `cause_text` берёт русский ярлык
    /// параметра из `param_label`, не внутренний `label_key`.
    #[test]
    fn cause_text_uses_param_label() {
        let text = cause_text(&Cause::PlayerSetting(ParamId::DsdAboveDac));
        assert_eq!(text, "настройка плеера: DSD выше возможностей ЦАП");
    }

    /// AM1.0-B27 (ТЗ-81, ТЗ-95, §6.25): `player_setting_text` берёт русский
    /// ярлык параметра из `param_label`, не внутренний `label_key`.
    #[test]
    fn player_setting_text_uses_param_label() {
        let rec = PlayerSettingRec::new(ParamId::Buffer, ModeKind::Optimal, RecAction::Increase)
            .expect("Buffer настраивается в Optimal");
        assert_eq!(recommendation_text(&Recommendation::PlayerSetting(rec)), "Буфер декодирование → вывод: увеличить");
    }

    /// AM1.0-B28 (ТЗ-80, ТЗ-81, §6.25): совет «Оборудование» — «ЦАП с
    /// поддержкой …», для DoP — с частотой PCM-контейнера (`dsd / 16`).
    #[test]
    fn hardware_recommendation_text_is_dac_support() {
        assert_eq!(
            recommendation_text(&Recommendation::Hardware { need: HwRequirement::Rate(sr(192_000)) }),
            "ЦАП с поддержкой 192 кГц"
        );
        assert_eq!(
            recommendation_text(&Recommendation::Hardware {
                need: HwRequirement::BitDepth(BitDepth::new(24).unwrap())
            }),
            "ЦАП с поддержкой 24 бит"
        );
        assert_eq!(
            recommendation_text(&Recommendation::Hardware { need: HwRequirement::Channels(6) }),
            "ЦАП с поддержкой 6 каналов"
        );
        assert_eq!(
            recommendation_text(&Recommendation::Hardware { need: HwRequirement::DopRate(DsdRate::Dsd256) }),
            "ЦАП с поддержкой DoP DSD256 (PCM 705,6 кГц)"
        );
    }
}
