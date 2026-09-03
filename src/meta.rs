use std::fs::File;
use std::path::Path;

use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::audio::decoder::{compute_bitrate, read_tags, Tags};

/// Byte offset where FLAC audio frames begin (i.e. end of the metadata block
/// chain). Accounts for an optional leading ID3v2 tag and the "fLaC" marker.
/// Returns `None` when the file is not a parseable FLAC.
pub(crate) fn flac_audio_offset(path: &Path) -> Option<u64> {
    use std::io::{BufReader, Read, Seek, SeekFrom};
    let mut f = BufReader::new(File::open(path).ok()?);
    let mut magic = [0u8; 10];
    let n = f.read(&mut magic).ok()?;
    let mut off: u64 = if n >= 10 && &magic[..3] == b"ID3" {
        let size = (u32::from(magic[6]) << 21)
            | (u32::from(magic[7]) << 14)
            | (u32::from(magic[8]) << 7)
            | u32::from(magic[9]);
        let id3_end = 10 + u64::from(size);
        f.seek(SeekFrom::Start(id3_end)).ok()?;
        let mut flac = [0u8; 4];
        if f.read_exact(&mut flac).is_err() || &flac != b"fLaC" {
            return None;
        }
        id3_end + 4
    } else if &magic[..4] == b"fLaC" {
        4
    } else {
        return None;
    };
    // Walk the metadata block chain; each header is 4 bytes at `off`.
    loop {
        f.seek(SeekFrom::Start(off)).ok()?;
        let mut hdr = [0u8; 4];
        if f.read_exact(&mut hdr).is_err() {
            return None;
        }
        let last = hdr[0] & 0x80 != 0;
        let len = ((u32::from(hdr[1]) << 16) | (u32::from(hdr[2]) << 8) | u32::from(hdr[3])) as u64;
        off += 4 + len;
        if last {
            return Some(off);
        }
    }
}

/// Average bitrate (kbps) derived from audio payload size and duration (the
/// real compressed data rate). For FLAC, tag/metadata overhead is excluded so
/// the value matches tooling such as mutagen / ffprobe.
pub(crate) fn audio_bitrate_from_file_size(
    path: &Path,
    num_frames: Option<u64>,
    sample_rate: u32,
    ext: &str,
) -> u32 {
    let frames = match num_frames {
        Some(f) => f,
        None => return 0,
    };
    if frames == 0 || sample_rate == 0 {
        return 0;
    }
    let duration = frames as f64 / f64::from(sample_rate);
    if duration <= 0.0 {
        return 0;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let payload = if ext.eq_ignore_ascii_case("flac") {
        match flac_audio_offset(path) {
            Some(o) if o < meta.len() => (meta.len() - o) as f64,
            _ => meta.len() as f64,
        }
    } else {
        meta.len() as f64
    };
    ((payload * 8.0) / (duration * 1000.0)) as u32
}

/// Container/format names keyed by extension (for the "Format" column).
pub fn format_name_for_ext(ext: &str) -> &'static str {
    match ext {
        "flac" | "fla" => "FLAC",
        "mp3" | "mp2" => "MP3",
        "wav" => "WAV",
        "ogg" | "oga" => "OGG",
        "opus" => "Opus",
        "m4a" | "m4b" | "aac" | "mp4" => "M4A/AAC",
        "alac" => "ALAC",
        "aiff" | "aif" => "AIFF",
        "dsf" => "DSF",
        "dff" => "DFF",
        _ => "Unknown",
    }
}

/// Lightweight probe result for a single audio file: tags, duration, and
/// technical info, read without fully constructing an audio decoder.
#[derive(Debug, Clone, Default)]
pub struct FileMeta {
    pub tags: Tags,
    pub duration: Option<f64>,
    pub sample_rate: u32,
    pub channels: u32,
    pub bitrate: u32,
    pub bits: Option<u32>,
    /// DSD rate label (e.g. "dsd64") for DSD tracks, else None.
    pub dsd_label: Option<String>,
}

impl FileMeta {
    pub fn bit_depth_string(&self) -> String {
        if let Some(label) = &self.dsd_label {
            return label.clone();
        }
        match self.bits {
            Some(b) if b > 0 => format!("{b} bit"),
            _ => String::new(),
        }
    }
}

fn probe_standard(path: &Path) -> FileMeta {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return FileMeta::default(),
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let fmt_opts: FormatOptions = Default::default();
    let meta_opts: MetadataOptions = Default::default();

    let mut format = match symphonia::default::get_probe().probe(&hint, mss, fmt_opts, meta_opts) {
        Ok(f) => f,
        Err(_) => return FileMeta::default(),
    };

    let tags = read_tags(&mut format);

    let mut duration = None;
    let mut sample_rate = 0u32;
    let mut channels = 0u32;
    let mut bits = None;
    let mut num_frames = None;
    if let Some(track) = format.default_track(TrackType::Audio) {
        if let Some(params) = track.codec_params.as_ref().and_then(|c| c.audio()) {
            num_frames = track.num_frames;
            let rate = params.sample_rate.unwrap_or(0);
            sample_rate = rate;
            channels = params
                .channels
                .as_ref()
                .map(|c| c.count() as u32)
                .unwrap_or(0);
            bits = params.bits_per_sample;
            if let (Some(n), true) = (track.num_frames, rate > 0) {
                duration = Some(n as f64 / rate as f64);
            }
        }
    }

    let bitrate = {
        let size_based = audio_bitrate_from_file_size(
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
            compute_bitrate(sample_rate, channels as usize, bits)
        }
    };

    FileMeta {
        tags,
        duration,
        sample_rate,
        channels,
        bitrate,
        bits,
        dsd_label: None,
    }
}

/// Probe a single file. DSD files use a dedicated lightweight parser because
/// symphonia cannot read them; everything else goes through symphonia's probe.
pub fn probe_file(path: &Path) -> FileMeta {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("dsf") || ext.eq_ignore_ascii_case("dff") {
        crate::audio::dsd::probe_dsd(path)
    } else {
        probe_standard(path)
    }
}
