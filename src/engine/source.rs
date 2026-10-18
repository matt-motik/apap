//! Открытие источника вне UI-потока (§2.10, §6.18, ТЗ-103, ADR-20).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::audio::decoder::{AudioSource, Tags, TrackInfo};
use crate::audio::error::FileError;
use crate::audio::format::{SampleBlock, SourceFormat, SourceKind};
use crate::audio::path::Route;

/// Открытие источника вне UI-потока (ТЗ-103, ADR-20, §6.18).
pub trait SourceOpener: Send + Sync {
    /// §6.18 шаг 3: только заголовок → `SourceFormat`, без seek-индекса;
    /// вызывается в служебном потоке открытия. Ошибка → `Skipped(File(..))`.
    fn probe(&self, path: &Path) -> Result<SourceFormat, FileError>;
    /// §6.18 шаг 6: полное открытие декодера внутри `apap-decode`.
    /// Ошибка → `SessionFailed` → пропуск с сообщением (ТЗ-86).
    fn open(&self, path: &Path) -> Result<Box<dyn AudioSource>, FileError>;
    /// §6.18 п. 6, ТЗ-113: открытие с учётом маршрута плана (`Route`) —
    /// `Dop` требует декодер DSD в режиме `DecodeMode::Dop`, `DsdToPcm` —
    /// тот же декодер в PCM-режиме. По умолчанию делегирует в [`Self::open`]
    /// без учёта маршрута (достаточно для фейков и прочих реализаций).
    fn open_for_route(&self, path: &Path, route: &Route) -> Result<Box<dyn AudioSource>, FileError> {
        let _ = route;
        self.open(path)
    }
}

/// Итог фейкового открытия: либо успешный формат, либо фиксированная ошибка
/// (§6.18, ADR-20).
enum FakeOutcome {
    Format(SourceFormat),
    Error(FileError),
}

/// Фейк `SourceOpener` для тестов: отдаёт заранее заданный `SourceFormat`
/// (и минимальный тихий `AudioSource` на `open`) либо заранее заданную
/// `FileError` на оба метода (§6.18 шаги 3 и 6, ТЗ-103, ADR-20).
pub struct FakeSource {
    outcome: FakeOutcome,
    probe_delay: Duration,
    probes: AtomicUsize,
    opens: AtomicUsize,
    /// Последний маршрут, переданный в `open_for_route` (§6.18 п. 6, ТЗ-113,
    /// тест `open_exclusive` против фейка).
    last_route: Mutex<Option<Route>>,
}

impl FakeSource {
    /// Успешный исход: `probe` и `open` работают с этим форматом.
    pub fn with_format(format: SourceFormat) -> FakeSource {
        FakeSource {
            outcome: FakeOutcome::Format(format),
            probe_delay: Duration::ZERO,
            probes: AtomicUsize::new(0),
            opens: AtomicUsize::new(0),
            last_route: Mutex::new(None),
        }
    }

    /// Исход-ошибка: `probe` и `open` возвращают эту ошибку (ТЗ-86).
    pub fn with_error(err: FileError) -> FakeSource {
        FakeSource {
            outcome: FakeOutcome::Error(err),
            probe_delay: Duration::ZERO,
            probes: AtomicUsize::new(0),
            opens: AtomicUsize::new(0),
            last_route: Mutex::new(None),
        }
    }

    /// Задержка внутри `probe`: тест медленного открытия
    /// `ui_handlers_do_not_block_on_slow_open` (§6.18).
    pub fn with_probe_delay(mut self, delay: Duration) -> FakeSource {
        self.probe_delay = delay;
        self
    }

    /// Сколько раз был вызван `probe`.
    pub fn probes(&self) -> usize {
        self.probes.load(Ordering::Acquire)
    }

    /// Сколько раз был вызван `open`.
    pub fn opens(&self) -> usize {
        self.opens.load(Ordering::Acquire)
    }

    /// Маршрут последнего вызова `open_for_route` (§6.18 п. 6, ТЗ-113).
    pub fn last_route(&self) -> Option<Route> {
        match self.last_route.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => None,
        }
    }
}

impl SourceOpener for FakeSource {
    fn probe(&self, _path: &Path) -> Result<SourceFormat, FileError> {
        self.probes.fetch_add(1, Ordering::AcqRel);
        if !self.probe_delay.is_zero() {
            std::thread::sleep(self.probe_delay);
        }
        match &self.outcome {
            FakeOutcome::Format(format) => Ok(format.clone()),
            FakeOutcome::Error(err) => Err(err.clone()),
        }
    }

    fn open(&self, _path: &Path) -> Result<Box<dyn AudioSource>, FileError> {
        self.opens.fetch_add(1, Ordering::AcqRel);
        match &self.outcome {
            FakeOutcome::Format(format) => Ok(Box::new(SilentSource::new(format))),
            FakeOutcome::Error(err) => Err(err.clone()),
        }
    }

    fn open_for_route(&self, path: &Path, route: &Route) -> Result<Box<dyn AudioSource>, FileError> {
        if let Ok(mut guard) = self.last_route.lock() {
            *guard = Some(route.clone());
        }
        self.open(path)
    }
}

/// Минимальный тихий `AudioSource`: сразу сигнализирует конец потока
/// (пустой поток без сэмплов), метаданные — из `SourceFormat` (§6.18).
struct SilentSource {
    info: TrackInfo,
    done: bool,
}

impl SilentSource {
    fn new(format: &SourceFormat) -> SilentSource {
        let bits = match format.kind {
            SourceKind::Pcm { bits } => Some(u32::from(bits.bits())),
            SourceKind::FloatPcm { double } => Some(if double { 64 } else { 32 }),
            SourceKind::Dsd { .. } => Some(1),
        };
        SilentSource {
            info: TrackInfo {
                sample_rate: format.rate.hz(),
                channels: usize::from(format.layout.count().get()),
                num_frames: Some(0),
                format_name: "fake".to_owned(),
                bitrate: 0,
                bits,
                tags: Tags::default(),
            },
            done: false,
        }
    }
}

impl AudioSource for SilentSource {
    fn next_block(&mut self) -> Result<Option<SampleBlock<'_>>, FileError> {
        self.done = true;
        Ok(None)
    }

    fn info(&self) -> &TrackInfo {
        &self.info
    }

    fn eof(&self) -> bool {
        self.done
    }
}

/// Продакшен-`SourceOpener` поверх существующих декодеров (§2.10, §6.18
/// шаги 3 и 6, ТЗ-103, ADR-20): Symphonia (`crate::audio::decoder`) для
/// обычных контейнеров, `crate::audio::dsd` для DSF/DFF по расширению пути.
///
/// Мост С3: `open` всегда отдаёт PCM-декодер DSD (`DsdDecoder::open`, режим
/// CIC) — DoP для живого воспроизведения остаётся внутри legacy `Player` до
/// отдельного этапа; здесь только маршрутизация по формату для `apap-decode`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SymphoniaSourceOpener;

impl SymphoniaSourceOpener {
    /// DSD-контейнер по расширению пути, без учёта регистра (DSF/DFF).
    fn is_dsd_path(path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(e) if e.eq_ignore_ascii_case("dsf") || e.eq_ignore_ascii_case("dff")
        )
    }

    /// Мост С3: `Decoder::open`/`DsdDecoder::open` сегодня возвращают
    /// `String`, а трейт `SourceOpener` — `FileError`. Если путь физически
    /// не открывается как файл — код ошибки `std::fs::metadata` заворачивается
    /// в `FileError::Io`; иначе текст декодера идёт в `Unsupported`. Позже
    /// декодеры будут отдавать `FileError` напрямую (§6.18 шаг 6), и этот
    /// мост уйдёт.
    fn map_open_error(path: &Path, msg: String) -> FileError {
        if let Err(e) = std::fs::metadata(path) {
            return FileError::Io(e.kind());
        }
        FileError::Unsupported { codec: msg }
    }
}

impl SourceOpener for SymphoniaSourceOpener {
    fn probe(&self, path: &Path) -> Result<SourceFormat, FileError> {
        if Self::is_dsd_path(path) {
            crate::audio::dsd::probe_dsd_format(path)
        } else {
            crate::audio::decoder::probe_header(path)
        }
    }

    fn open(&self, path: &Path) -> Result<Box<dyn AudioSource>, FileError> {
        if Self::is_dsd_path(path) {
            crate::audio::dsd::DsdDecoder::open(path)
                .map(|d| Box::new(d) as Box<dyn AudioSource>)
                .map_err(|msg| Self::map_open_error(path, msg))
        } else {
            crate::audio::decoder::Decoder::open(path)
                .map(|d| Box::new(d) as Box<dyn AudioSource>)
                .map_err(|msg| Self::map_open_error(path, msg))
        }
    }

    /// Открытие по маршруту плана (§6.18 п. 6, ТЗ-113): `Route::Dop` —
    /// декодер DSD в режиме `DecodeMode::Dop` (сырые DSD-байты в DoP-кадрах);
    /// `Route::DsdToPcm` — тот же декодер в штатном режиме `DecodeMode::Cic`
    /// (ОВС-6, ОВС-6б), как и обычный [`Self::open`] для DSD-файлов — декодер
    /// сегодня не принимает `pcm_rate`/`filter`/`stop_hz` маршрута, PCM-темп
    /// всегда `dsd_rate / 64`. `Route::Pcm` не меняет поведение [`Self::open`].
    fn open_for_route(&self, path: &Path, route: &Route) -> Result<Box<dyn AudioSource>, FileError> {
        match route {
            Route::Dop(_) => crate::audio::dsd::DsdDecoder::open_with_mode(path, crate::audio::dsd::DecodeMode::Dop)
                .map(|d| Box::new(d) as Box<dyn AudioSource>)
                .map_err(|msg| Self::map_open_error(path, msg)),
            Route::Pcm | Route::DsdToPcm { .. } => self.open(path),
        }
    }
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FakeSource>();
    assert_send_sync::<SymphoniaSourceOpener>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::audio::backend::AccessKind;
    use crate::audio::format::{ChannelLayout, Codec, Container, SampleRate};
    use crate::audio::path::DopRoute;
    use std::path::{Path, PathBuf};

    fn data_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
    }

    fn pcm_format() -> SourceFormat {
        SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: crate::audio::format::BitDepth::new(16).expect("bits") },
        }
    }

    #[test]
    fn fake_source_counts_probe_and_open() {
        let format = pcm_format();
        let fake = FakeSource::with_format(format.clone());
        let path = Path::new("fake.flac");

        let probed = fake.probe(path).expect("probe ok");
        assert_eq!(probed, format);

        let opened = fake.open(path).expect("open ok");
        assert_eq!(opened.info().sample_rate, 44_100);
        assert_eq!(opened.info().channels, 2);

        assert_eq!(fake.probes(), 1);
        assert_eq!(fake.opens(), 1);
    }

    #[test]
    fn fake_source_reports_file_error() {
        let err = FileError::Unsupported { codec: "weird".to_owned() };
        let fake = FakeSource::with_error(err.clone());
        let path = Path::new("broken.flac");

        assert_eq!(fake.probe(path), Err(err.clone()));
        assert_eq!(fake.open(path).err(), Some(err));

        assert_eq!(fake.probes(), 1);
        assert_eq!(fake.opens(), 1);
    }

    #[test]
    fn symphonia_opener_probes_and_opens_dsf() {
        let opener = SymphoniaSourceOpener;
        let path = data_path("dsf_dsd64_1k.dsf");

        let format = opener.probe(&path).expect("probe ok");
        assert_eq!(format.container, Container::Dsf);

        let source = opener.open(&path).expect("open ok");
        assert!(source.info().channels > 0);
    }

    #[test]
    fn symphonia_opener_probes_pcm_fixture() {
        let opener = SymphoniaSourceOpener;
        let path = data_path("flac_16_44k1_md5.flac");

        let format = opener.probe(&path).expect("probe ok");
        assert_eq!(format.container, Container::Flac);

        let source = opener.open(&path).expect("open ok");
        assert_eq!(source.info().sample_rate, 44_100);
    }

    #[test]
    fn symphonia_opener_missing_file_is_io_error() {
        let opener = SymphoniaSourceOpener;
        let path = Path::new("/nonexistent/apap-test/does_not_exist.flac");

        assert_eq!(opener.probe(path), Err(FileError::Io(std::io::ErrorKind::NotFound)));
        assert_eq!(opener.open(path).err(), Some(FileError::Io(std::io::ErrorKind::NotFound)));
    }

    /// §6.18 п. 6, ТЗ-113: `Route::Dop` открывает DSD-декодер в режиме DoP
    /// (`DecodeMode::Dop`), а не в штатном PCM-режиме `open()`.
    #[test]
    fn symphonia_opener_open_for_route_dop_uses_dop_decode_mode() {
        let opener = SymphoniaSourceOpener;
        let path = data_path("dsf_dsd64_1k.dsf");

        let format = opener.probe(&path).expect("probe ok");
        let SourceKind::Dsd { dsd } = format.kind else {
            panic!("expected DSD format");
        };
        let route = Route::Dop(
            DopRoute::new(dsd, SampleRate::new(44_100).expect("rate"), AccessKind::Exclusive).expect("dop route"),
        );

        let source = opener.open_for_route(&path, &route).expect("open ok");
        assert!(source.info().channels > 0);
    }

    /// §6.18 п. 6, ТЗ-113: реализация по умолчанию `open_for_route`
    /// делегирует в `open` независимо от маршрута — достаточно для фейков,
    /// не наблюдающих маршрут.
    #[test]
    fn default_open_for_route_delegates_to_open() {
        struct DefaultOnlyOpener(SourceFormat);

        impl SourceOpener for DefaultOnlyOpener {
            fn probe(&self, _path: &Path) -> Result<SourceFormat, FileError> {
                Ok(self.0.clone())
            }
            fn open(&self, _path: &Path) -> Result<Box<dyn AudioSource>, FileError> {
                Ok(Box::new(SilentSource::new(&self.0)))
            }
        }

        let opener = DefaultOnlyOpener(pcm_format());
        let path = Path::new("fake.flac");

        let source = opener.open_for_route(path, &Route::Pcm).expect("open ok");
        assert_eq!(source.info().sample_rate, 44_100);
    }
}
