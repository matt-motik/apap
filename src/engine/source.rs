//! Открытие источника вне UI-потока (§2.10, §6.18, ТЗ-103, ADR-20).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::audio::decoder::{AudioSource, Tags, TrackInfo};
use crate::audio::error::FileError;
use crate::audio::format::{SampleBlock, SourceFormat, SourceKind};

/// Открытие источника вне UI-потока (ТЗ-103, ADR-20, §6.18).
pub trait SourceOpener: Send + Sync {
    /// §6.18 шаг 3: только заголовок → `SourceFormat`, без seek-индекса;
    /// вызывается в служебном потоке открытия. Ошибка → `Skipped(File(..))`.
    fn probe(&self, path: &Path) -> Result<SourceFormat, FileError>;
    /// §6.18 шаг 6: полное открытие декодера внутри `apap-decode`.
    /// Ошибка → `SessionFailed` → пропуск с сообщением (ТЗ-86).
    fn open(&self, path: &Path) -> Result<Box<dyn AudioSource>, FileError>;
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
}

impl FakeSource {
    /// Успешный исход: `probe` и `open` работают с этим форматом.
    pub fn with_format(format: SourceFormat) -> FakeSource {
        FakeSource {
            outcome: FakeOutcome::Format(format),
            probe_delay: Duration::ZERO,
            probes: AtomicUsize::new(0),
            opens: AtomicUsize::new(0),
        }
    }

    /// Исход-ошибка: `probe` и `open` возвращают эту ошибку (ТЗ-86).
    pub fn with_error(err: FileError) -> FakeSource {
        FakeSource {
            outcome: FakeOutcome::Error(err),
            probe_delay: Duration::ZERO,
            probes: AtomicUsize::new(0),
            opens: AtomicUsize::new(0),
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

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FakeSource>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::audio::format::{ChannelLayout, Codec, Container, SampleRate};
    use std::path::Path;

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
}
