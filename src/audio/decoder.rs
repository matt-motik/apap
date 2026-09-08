use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

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
/// Core operations: `next_frames`, `info`, `eof`. `duration_secs` and `seek`
/// are optional and ship with safe defaults (duration derived from
/// `info().num_frames`, seek unsupported), so a source that cannot know its
/// length or is non-seekable implements only the core three methods.
pub trait AudioSource: Send {
    /// Next interleaved f32 block, or `None` at end of stream / on error.
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

pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    pub info: TrackInfo,
    pub eof: bool,
    scratch: Vec<f32>,
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
        })
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
        let nanos = frac.min(999_999_999.0).max(0.0) as u32;
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
    fn optional_methods_have_safe_defaults() {
        struct NoSeek {
            info: TrackInfo,
        }
        impl AudioSource for NoSeek {
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
