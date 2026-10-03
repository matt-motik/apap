use std::fs::File;
use std::io::ErrorKind;
use std::path::Path;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_PCM_F32BE, CODEC_ID_PCM_F32BE_PLANAR, CODEC_ID_PCM_F32LE, CODEC_ID_PCM_F32LE_PLANAR,
    CODEC_ID_PCM_F64BE, CODEC_ID_PCM_F64BE_PLANAR, CODEC_ID_PCM_F64LE, CODEC_ID_PCM_F64LE_PLANAR,
};
use symphonia::core::codecs::audio::{AudioCodecId, AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use super::error::{CorruptKind, FileError};
use super::format::{BitDepth, SampleBlock};

#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub sample_rate: u32,
    pub channels: usize,
    pub num_frames: Option<u64>,
    pub format_name: String,
    pub bitrate: u32,
    pub bits: Option<u32>,
    pub tags: Tags,
}

/// Unified source interface shared by the symphonia decoder and the DSD
/// decoder, so the playback pipeline is driven identically for both.
///
/// Core operations: `next_block`, `info`, `eof`. `duration_secs` and `seek`
/// are optional and ship with safe defaults (duration derived from
/// `info().num_frames`, seek unsupported), so a source that cannot know its
/// length or is non-seekable implements only the core three methods.
pub trait AudioSource: Send {
    /// Next interleaved block in the source's native representation (§6.10, ADR-03):
    /// `Ok(None)` at end of stream, `Err` on a corrupt stream or a read error.
    fn next_block(&mut self) -> Result<Option<SampleBlock<'_>>, FileError>;

    /// Next interleaved f32 block, or `None` at end of stream / on error.
    /// Old pipeline only: removed together with the f32 worker (§8 С2).
    fn next_frames(&mut self) -> Option<&[f32]>;

    /// Seek the source to `secs` (0.0 = start). Default: unsupported.
    fn seek(&mut self, _secs: f64) -> Result<(), String> {
        Err("Seek is not supported by this audio source".into())
    }

    /// Track duration in seconds when knowable. Default derives it from
    /// `info().num_frames`; override when decoding spoils the rate reported
    /// through `info` (none of the built-in sources need to).
    fn duration_secs(&self) -> Option<f64> {
        let info = self.info();
        info.num_frames.map(|n| n as f64 / info.sample_rate as f64)
    }

    /// Static track metadata.
    fn info(&self) -> &TrackInfo;

    /// True once the source has delivered its last frame (natural end).
    fn eof(&self) -> bool;
}

#[derive(Debug, Clone, Default)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub year: Option<String>,
    pub track_number: u32,
    pub track_total: u32,
    pub disc_number: u32,
    pub disc_total: u32,
}

fn string_value(v: &symphonia::core::meta::RawValue) -> Option<String> {
    match v {
        symphonia::core::meta::RawValue::String(s) => Some(s.to_string()),
        symphonia::core::meta::RawValue::StringList(l) => l.first().cloned(),
        _ => None,
    }
}

pub(crate) fn read_tags(format: &mut Box<dyn FormatReader>) -> Tags {
    let mut tags = Tags::default();
    let meta = format.metadata();
    let Some(rev) = meta.current() else {
        return tags;
    };
    for tag in &rev.media.tags {
        let key = tag.raw.key.to_ascii_lowercase();
        if let Some(value) = string_value(&tag.raw.value) {
            match key.as_str() {
                "title" | "tracktitle" | "titre" => {
                    if tags.title.is_none() {
                        tags.title = Some(value);
                    }
                }
                "artist" | "tpe1" | "tp1" => {
                    if tags.artist.is_none() {
                        tags.artist = Some(value);
                    }
                }
                "albumartist" | "album_artist" | "album artist" | "tpe2" => {
                    if tags.artist.is_none() {
                        tags.artist = Some(value.clone());
                    }
                    if tags.album_artist.is_none() {
                        tags.album_artist = Some(value);
                    }
                }
                "album" | "talbum" | "tal" => {
                    if tags.album.is_none() {
                        tags.album = Some(value);
                    }
                }
                "genre" | "tcon" => {
                    if tags.genre.is_none() {
                        tags.genre = Some(value);
                    }
                }
                "date" | "year" | "tyer" | "tdrc" | "tory" => {
                    if tags.year.is_none() {
                        tags.year = Some(year_from_date(&value));
                    }
                }
                "tracknumber" | "trck" | "trk" => {
                    let (n, t) = parse_number_pair(&value);
                    if tags.track_number == 0 {
                        tags.track_number = n;
                    }
                    tags.track_total = tags.track_total.max(t);
                }
                "discnumber" | "disk" | "tpa" => {
                    let (n, t) = parse_number_pair(&value);
                    if tags.disc_number == 0 {
                        tags.disc_number = n;
                    }
                    tags.disc_total = tags.disc_total.max(t);
                }
                _ => {}
            }
        }
        if let Some(std) = &tag.std {
            use symphonia::core::meta::StandardTag as S;
            match std {
                S::Artist(v) => {
                    if tags.artist.is_none() {
                        tags.artist = Some(v.to_string());
                    }
                }
                S::AlbumArtist(v) => {
                    if tags.artist.is_none() {
                        tags.artist = Some(v.to_string());
                    }
                    if tags.album_artist.is_none() {
                        tags.album_artist = Some(v.to_string());
                    }
                }
                S::Album(v) => {
                    if tags.album.is_none() {
                        tags.album = Some(v.to_string());
                    }
                }
                S::Genre(v) => {
                    if tags.genre.is_none() {
                        tags.genre = Some(v.to_string());
                    }
                }
                S::ReleaseDate(v) => {
                    if tags.year.is_none() {
                        tags.year = Some(year_from_date(v.as_str()));
                    }
                }
                S::TrackNumber(v) => {
                    if tags.track_number == 0 {
                        tags.track_number = *v as u32;
                    }
                }
                S::TrackTotal(v) => {
                    tags.track_total = tags.track_total.max(*v as u32);
                }
                S::DiscNumber(v) => {
                    if tags.disc_number == 0 {
                        tags.disc_number = *v as u32;
                    }
                }
                S::DiscTotal(v) => {
                    tags.disc_total = tags.disc_total.max(*v as u32);
                }
                _ => {}
            }
        }
    }
    tags
}

/// Parse a `"N"` or `"N/M"` pair (track/disc numbers) into (number, total).
fn parse_number_pair(s: &str) -> (u32, u32) {
    let s = s.trim();
    if s.is_empty() {
        return (0, 0);
    }
    let mut parts = s.split('/');
    let num = parts
        .next()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let total = parts
        .next()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(0);
    (num, total)
}

/// Extract a 4-digit year from a date string (e.g. "2019-05-01" -> "2019").
fn year_from_date(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 4 && t[..4].chars().all(|c| c.is_ascii_digit()) {
        t[..4].to_string()
    } else {
        t.to_string()
    }
}

/// Разрядность как константа: `BitDepth::new` вычисляется при компиляции.
const fn depth(bits: u8) -> BitDepth {
    match BitDepth::new(bits) {
        Some(b) => b,
        None => panic!("разрядность вне 1..=32"),
    }
}

const BITS_8: BitDepth = depth(8);
const BITS_16: BitDepth = depth(16);
const BITS_24: BitDepth = depth(24);
const BITS_32: BitDepth = depth(32);

/// float-PCM файла (WAV/AIFF float32/float64): отдаётся как `SampleBlock::F32` (ОВС-1).
fn is_float_pcm(codec: AudioCodecId) -> bool {
    [
        CODEC_ID_PCM_F32LE,
        CODEC_ID_PCM_F32LE_PLANAR,
        CODEC_ID_PCM_F32BE,
        CODEC_ID_PCM_F32BE_PLANAR,
        CODEC_ID_PCM_F64LE,
        CODEC_ID_PCM_F64LE_PLANAR,
        CODEC_ID_PCM_F64BE,
        CODEC_ID_PCM_F64BE_PLANAR,
    ]
    .contains(&codec)
}

/// `valid_bits` целого варианта symphonia: собственная ширина варианта, но не больше
/// `bits_per_sample` из `codec_params` — FLAC отдаёт `S32` уже со сдвигом влево (§6.10).
fn int_valid_bits(native: BitDepth, bits_per_sample: Option<u32>) -> BitDepth {
    bits_per_sample
        .and_then(|b| u8::try_from(b).ok())
        .and_then(BitDepth::new)
        .map_or(native, |b| b.min(native))
}

/// Округление lossy-выхода до 24 бит без дизеринга (ОВ-36, §6.10):
/// `round(x · 2²³)` (ties away from zero), ограничение границами i24, `<< 8`.
/// Второе значение — `clamp` изменил сэмпл (счётчик `lossy_clipped`, ОВС-11). NaN → 0.
pub(crate) fn round_lossy_24(x: f64) -> (i32, bool) {
    let y = (x * 8_388_608.0).round();
    if y.is_nan() {
        return (0, false);
    }
    let c = y.clamp(-8_388_608.0, 8_388_607.0);
    // После `clamp` значение в диапазоне i24: приведение точное, сдвиг не переполняет.
    ((c as i32) << 8, c != y)
}

/// Что положено во внутренний буфер очередным пакетом.
enum Filled {
    I32 { len: usize, valid_bits: BitDepth },
    F32 { len: usize },
}

pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    pub info: TrackInfo,
    pub eof: bool,
    scratch: Vec<f32>,
    /// Буфер `ExactI32` (§6.10), выделен при открытии: `max_frames_per_packet × каналы`.
    buf_i32: Vec<i32>,
    /// Буфер `F32` для float-PCM (ОВС-1).
    buf_f32: Vec<f32>,
    /// Промежуточный float-выход lossy-кодека перед округлением до 24 бит.
    buf_lossy: Vec<f64>,
    float_pcm: bool,
    bits_per_sample: Option<u32>,
    frames_decoded: u64,
    decode_errors: u64,
    lossy_clipped: u64,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Decoder, String> {
        let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let mut fmt_opts: FormatOptions = Default::default();
        fmt_opts.prebuild_seek_index = true;
        fmt_opts.seek_index_fill_period_ms = 500;
        let meta_opts: MetadataOptions = Default::default();

        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, fmt_opts, meta_opts)
            .map_err(|e| format!("Unsupported or unreadable format: {e}"))?;

        let tags = read_tags(&mut format);

        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| String::from("No audio track found"))?;

        let track_id = track.id;
        let format_name = format.format_info().long_name.to_string();
        let params = track
            .codec_params
            .as_ref()
            .and_then(|c| c.audio())
            .ok_or_else(|| String::from("Track has no audio codec parameters"))?;

        let sample_rate = params.sample_rate.unwrap_or(44100);
        let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
        let num_frames = track.num_frames;
        let bits = params.bits_per_sample;
        let float_pcm = is_float_pcm(params.codec);
        let block_cap = params
            .max_frames_per_packet
            .and_then(|f| usize::try_from(f).ok())
            .unwrap_or(0)
            .saturating_mul(channels);

        let mut dec_opts = AudioDecoderOptions::default();
        dec_opts.gapless = true;
        dec_opts.verify = false;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &dec_opts)
            .map_err(|e| format!("Unsupported codec: {e}"))?;

        // Average bitrate in kbps: prefer the actual payload / duration
        // (compressed) rate; fall back to the uncompressed PCM byte-rate only
        // when no frame count is available.
        let bitrate = {
            let size_based = crate::meta::audio_bitrate_from_file_size(
                path,
                num_frames,
                sample_rate,
                &path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase(),
            );
            if size_based > 0 {
                size_based
            } else {
                compute_bitrate(sample_rate, channels, bits)
            }
        };

        Ok(Decoder {
            format,
            decoder,
            track_id,
            info: TrackInfo {
                sample_rate,
                channels,
                num_frames,
                format_name,
                bitrate,
                bits,
                tags,
            },
            eof: false,
            scratch: Vec::new(),
            buf_i32: if float_pcm { Vec::new() } else { vec![0; block_cap] },
            buf_f32: if float_pcm { vec![0.0; block_cap] } else { Vec::new() },
            buf_lossy: Vec::new(),
            float_pcm,
            bits_per_sample: bits,
            frames_decoded: 0,
            decode_errors: 0,
            lossy_clipped: 0,
        })
    }

    /// Пропущенных повреждённых пакетов (ТЗ-87, ТЗ-75).
    pub fn decode_errors(&self) -> u64 {
        self.decode_errors
    }

    /// Сэмплов lossy-выхода, ограниченных границами 24 бит (ОВС-11); поток
    /// декодирования публикует значение в `SessionShared::lossy_clipped`.
    pub fn lossy_clipped(&self) -> u64 {
        self.lossy_clipped
    }

    /// Ошибка чтения потока (ТЗ-87, §6.10): `UnexpectedEof` — естественный конец (`Ok`),
    /// прочий ввод-вывод — `ReadDuringPlayback`, остальное — `Corrupt`.
    fn stream_error(&mut self, e: SymError) -> Result<(), FileError> {
        self.eof = true;
        match e {
            SymError::IoError(io) if io.kind() == ErrorKind::UnexpectedEof => Ok(()),
            SymError::IoError(io) => Err(FileError::ReadDuringPlayback {
                at_frame: self.frames_decoded,
                kind: io.kind(),
            }),
            _ => Err(FileError::Corrupt(CorruptKind::DecodeFailed)),
        }
    }

    /// Следующий блок тракта (§6.10, ADR-03, ТЗ-4…ТЗ-6): целые источники — `ExactI32`
    /// исходной разрядности, выровненные влево; lossy — `ExactI32` 24 бит после
    /// округления (ОВ-36); float-PCM — `F32` (ОВС-1). `Ok(None)` — конец потока.
    /// Повреждённый пакет пропускается с `decode_errors += 1`.
    pub fn next_block(&mut self) -> Result<Option<SampleBlock<'_>>, FileError> {
        if self.eof {
            return Ok(None);
        }
        let filled = loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.eof = true;
                    return Ok(None);
                }
                Err(e) => return self.stream_error(e).map(|()| None),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let buf = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                Err(SymError::DecodeError(_)) => {
                    self.decode_errors = self.decode_errors.saturating_add(1);
                    continue;
                }
                Err(e) => return self.stream_error(e).map(|()| None),
            };
            let len = buf.samples_interleaved();
            if len == 0 {
                continue;
            }
            self.frames_decoded = self
                .frames_decoded
                .saturating_add(u64::try_from(buf.frames()).unwrap_or(u64::MAX));
            let native = match buf {
                GenericAudioBufferRef::U8(_) | GenericAudioBufferRef::S8(_) => Some(BITS_8),
                GenericAudioBufferRef::U16(_) | GenericAudioBufferRef::S16(_) => Some(BITS_16),
                GenericAudioBufferRef::U24(_) | GenericAudioBufferRef::S24(_) => Some(BITS_24),
                GenericAudioBufferRef::U32(_) | GenericAudioBufferRef::S32(_) => Some(BITS_32),
                GenericAudioBufferRef::F32(_) | GenericAudioBufferRef::F64(_) => None,
            };
            if let Some(native) = native {
                // Целые → i32 в symphonia — чистые сдвиги влево (беззнаковые — с инверсией
                // старшего бита); младшие 32 − valid_bits бит нулевые (И-Р4).
                if self.buf_i32.len() < len {
                    self.buf_i32.resize(len, 0);
                }
                buf.copy_to_slice_interleaved(&mut self.buf_i32[..len]);
                break Filled::I32 {
                    len,
                    valid_bits: int_valid_bits(native, self.bits_per_sample),
                };
            }
            if self.float_pcm {
                // F32 — копия без изменений, F64 — `as f32` (FloatNarrow, ОВС-1).
                if self.buf_f32.len() < len {
                    self.buf_f32.resize(len, 0.0);
                }
                buf.copy_to_slice_interleaved(&mut self.buf_f32[..len]);
                break Filled::F32 { len };
            }
            if self.buf_lossy.len() < len {
                self.buf_lossy.resize(len, 0.0);
            }
            if self.buf_i32.len() < len {
                self.buf_i32.resize(len, 0);
            }
            buf.copy_to_slice_interleaved(&mut self.buf_lossy[..len]);
            let mut clipped = 0u64;
            for (d, &x) in self.buf_i32.iter_mut().zip(&self.buf_lossy[..len]) {
                let (s, c) = round_lossy_24(x);
                *d = s;
                clipped += u64::from(c);
            }
            self.lossy_clipped = self.lossy_clipped.saturating_add(clipped);
            break Filled::I32 {
                len,
                valid_bits: BITS_24,
            };
        };
        Ok(Some(match filled {
            Filled::I32 { len, valid_bits } => SampleBlock::ExactI32 {
                data: &self.buf_i32[..len],
                valid_bits,
            },
            Filled::F32 { len } => SampleBlock::F32 {
                data: &self.buf_f32[..len],
            },
        }))
    }

    pub fn info(&self) -> &TrackInfo {
        &self.info
    }

    pub fn eof(&self) -> bool {
        self.eof
    }

    /// Decode the next packet into `out` (interleaved f32) and return the number of
    /// source frames written. Returns `None` at end of stream or on unrecoverable error.
    pub fn next_frames(&mut self) -> Option<&[f32]> {
        if self.eof {
            return None;
        }
        loop {
            match self.format.next_packet() {
                Ok(Some(packet)) => {
                    if packet.track_id != self.track_id {
                        continue;
                    }
                    match self.decoder.decode(&packet) {
                        Ok(buf) => {
                            let total = buf.samples_interleaved();
                            if total == 0 {
                                continue;
                            }
                            self.scratch.resize(total, 0.0);
                            buf.copy_to_slice_interleaved(&mut self.scratch);
                            return Some(self.scratch.as_slice());
                        }
                        Err(SymError::DecodeError(_)) => continue,
                        Err(_) => {
                            self.eof = true;
                            return None;
                        }
                    }
                }
                Ok(None) => {
                    self.eof = true;
                    return None;
                }
                Err(_) => {
                    self.eof = true;
                    return None;
                }
            }
        }
    }

    pub fn seek(&mut self, secs: f64) -> Result<(), String> {
        let secs = secs.max(0.0);
        let whole = secs.floor() as i64;
        let frac = (secs.fract() * 1e9).round();
        let nanos = frac.clamp(0.0, 999_999_999.0) as u32;
        let time = Time::try_new(whole, nanos).unwrap_or(Time::ZERO);
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| format!("Seek failed: {e}"))?;
        self.decoder.reset();
        self.eof = false;
        Ok(())
    }
}

/// Uncompressed PCM bitrate (kbps) from codec parameters; used as a fallback
/// when file-size information is unavailable.
pub(crate) fn compute_bitrate(sample_rate: u32, channels: usize, bits: Option<u32>) -> u32 {
    let rate = u64::from(sample_rate);
    let bps = u64::from(bits.unwrap_or(16));
    if rate == 0 || channels == 0 {
        return 0;
    }
    ((rate * channels as u64 * bps) / 1000) as u32
}

impl AudioSource for Decoder {
    fn next_block(&mut self) -> Result<Option<SampleBlock<'_>>, FileError> {
        Decoder::next_block(self)
    }
    fn next_frames(&mut self) -> Option<&[f32]> {
        Decoder::next_frames(self)
    }
    fn seek(&mut self, secs: f64) -> Result<(), String> {
        Decoder::seek(self, secs)
    }

    fn info(&self) -> &TrackInfo {
        Decoder::info(self)
    }

    fn eof(&self) -> bool {
        Decoder::eof(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn decode_and_seek_headless() {
        let Some(path) = std::env::var_os("MUSIC_TEST_FILE") else {
            eprintln!("skipped: MUSIC_TEST_FILE not set");
            return;
        };
        let path = PathBuf::from(path);
        let mut dec = Decoder::open(&path).expect("open");
        assert!(dec.info.sample_rate > 0);
        assert!(dec.info.channels > 0);
        assert!(
            dec.info.tags.artist.is_some() || dec.info.tags.title.is_some(),
            "expected readable tags: {:#?}",
            dec.info.tags
        );

        let ch = dec.info.channels;
        let rate = dec.info.sample_rate;
        let mut total = 0usize;
        let mut finite = true;
        for _ in 0..200 {
            match dec.next_frames() {
                Some(buf) => {
                    total += buf.len() / ch;
                    finite &= buf.iter().all(|s| s.is_finite());
                    if total >= rate as usize {
                        break;
                    }
                }
                None => break,
            }
        }
        assert!(total > 0, "no audio produced");
        assert!(finite, "non-finite samples");

        dec.seek(30.0).expect("seek");
        let mut saw = false;
        for _ in 0..100 {
            if let Some(buf) = dec.next_frames() {
                if !buf.is_empty() {
                    saw = true;
                    break;
                }
            }
        }
        assert!(saw, "no audio after seek");
    }

    #[test]
    fn lossy_decoder_rounds_to_24bit() {
        let lsb = 1.0 / 8_388_608.0;
        assert_eq!(round_lossy_24(0.0), (0, false));
        assert_eq!(round_lossy_24(0.5), (0x40_0000 << 8, false));
        assert_eq!(round_lossy_24(-0.5), (-0x40_0000 << 8, false));
        // Ties away from zero.
        assert_eq!(round_lossy_24(0.5 * lsb), (1 << 8, false));
        assert_eq!(round_lossy_24(-0.5 * lsb), (-1 << 8, false));
        assert_eq!(round_lossy_24(0.49 * lsb), (0, false));
        // Границы i24: −1.0 точно, +1.0 и перегрузка ограничиваются со счётом.
        assert_eq!(round_lossy_24(-1.0), (-8_388_608 << 8, false));
        assert_eq!(round_lossy_24(1.0), (8_388_607 << 8, true));
        assert_eq!(round_lossy_24(3.5), (8_388_607 << 8, true));
        assert_eq!(round_lossy_24(-1.0 - lsb), (-8_388_608 << 8, true));
        assert_eq!(round_lossy_24(f64::NAN), (0, false));
        // Младшие 8 бит всегда нулевые (И-Р4).
        for i in -1000..1000 {
            let (s, _) = round_lossy_24(f64::from(i) * 0.001_37);
            assert_eq!(s & 0xFF, 0);
        }
    }

    /// Минимальный WAV (`fmt ` + `data`) во временный файл.
    fn write_wav(name: &str, format_tag: u16, bits: u16, channels: u16, data: &[u8]) -> PathBuf {
        let rate = 44_100u32;
        let block = channels * bits / 8;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&format_tag.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
        b.extend_from_slice(&block.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
        let path = std::env::temp_dir().join(format!("{}-{name}", std::process::id()));
        std::fs::write(&path, b).expect("write wav");
        path
    }

    #[test]
    fn next_block_i16_wav_is_exact_left_aligned() {
        let src: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 0x1234, -0x1234, 7];
        let data: Vec<u8> = src.iter().flat_map(|s| s.to_le_bytes()).collect();
        let path = write_wav("i16.wav", 1, 16, 2, &data);
        let mut dec = Decoder::open(&path).expect("open");
        let mut got = Vec::new();
        while let Some(block) = dec.next_block().expect("read") {
            match block {
                SampleBlock::ExactI32 { data, valid_bits } => {
                    assert_eq!(valid_bits, BITS_16);
                    got.extend_from_slice(data);
                }
                other => panic!("ожидался ExactI32: {other:?}"),
            }
        }
        let want: Vec<i32> = src.iter().map(|&s| i32::from(s) << 16).collect();
        assert_eq!(got, want);
        assert!(dec.eof());
        assert_eq!(dec.decode_errors(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn next_block_f32_wav_is_float_copy() {
        let src: Vec<f32> = vec![0.0, 0.5, -1.0, 1.5, 1e-9, -0.25];
        let data: Vec<u8> = src.iter().flat_map(|s| s.to_le_bytes()).collect();
        let path = write_wav("f32.wav", 3, 32, 2, &data);
        let mut dec = Decoder::open(&path).expect("open");
        let mut got = Vec::new();
        while let Some(block) = dec.next_block().expect("read") {
            match block {
                SampleBlock::F32 { data } => got.extend_from_slice(data),
                other => panic!("ожидался F32: {other:?}"),
            }
        }
        // Побитово, включая значения вне [-1, 1) (ОВС-1).
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&got), bits(&src));
        assert_eq!(dec.lossy_clipped(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn valid_bits_capped_by_bits_per_sample() {
        // FLAC 24 бит в S32: valid_bits из bits_per_sample.
        assert_eq!(int_valid_bits(BITS_32, Some(24)), BITS_24);
        assert_eq!(int_valid_bits(BITS_16, Some(24)), BITS_16);
        assert_eq!(int_valid_bits(BITS_16, None), BITS_16);
        assert_eq!(int_valid_bits(BITS_32, Some(0)), BITS_32);
        assert_eq!(int_valid_bits(BITS_32, Some(64)), BITS_32);
    }

    #[test]
    fn float_pcm_codecs_detected() {
        assert!(is_float_pcm(CODEC_ID_PCM_F32LE));
        assert!(is_float_pcm(CODEC_ID_PCM_F64BE_PLANAR));
        assert!(!is_float_pcm(
            symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC
        ));
    }

    #[test]
    fn optional_methods_have_safe_defaults() {
        struct NoSeek {
            info: TrackInfo,
        }
        impl AudioSource for NoSeek {
            fn next_block(&mut self) -> Result<Option<SampleBlock<'_>>, FileError> {
                Ok(None)
            }
            fn next_frames(&mut self) -> Option<&[f32]> {
                None
            }
            fn info(&self) -> &TrackInfo {
                &self.info
            }
            fn eof(&self) -> bool {
                false
            }
        }
        let src = NoSeek {
            info: TrackInfo {
                sample_rate: 44100,
                channels: 2,
                num_frames: Some(44_100),
                format_name: "none".into(),
                bitrate: 0,
                bits: None,
                tags: Tags::default(),
            },
        };
        // `duration_secs` derived from `info().num_frames` by default.
        assert_eq!(src.duration_secs(), Some(1.0));
        // `seek` fails cleanly instead of being unimplementable.
        let mut src = src;
        assert!(src.seek(10.0).is_err());
    }
}
