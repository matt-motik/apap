//! DSD → PCM decoding for DSF and DSDIFF (DFF) containers.
//!
//! Container notes (offsets verified against dsf-meta 0.3.0 / dff-meta 0.2.0):
//! * DSF:   planar channels (block of `block_size` bytes per channel), sample
//!   data at offset 92, bits LSB-first (bit 0 = earliest sample). A metadata
//!   pointer at offset 20 locates an optional ID3v2 tag.
//! * DFF:   channel-interleaved bytes, sample data starts right after the
//!   12-byte `DSD ` chunk header, bits MSB-first (bit 7 = earliest sample).
//!   Sample rate lives in the `FS  ` sub-chunk of `PROP/SND `; DST encode is
//!   detected via `CMPR` and rejected. An `ID3 ` chunk may sit after the audio.
//!
//! The 1-bit stream is decimated by 64 (two cascaded 4th-order CIC stages by
//! 8) to PCM: 44100/48000 Hz for DSD64, 88200/96000 for DSD128, etc.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use super::decoder::{AudioSource, Tags, TrackInfo};

const CIC_DECIM: u8 = 8;
const STAGE_GAIN: f64 = 4096.0; // 8^4
const TOTAL_GAIN: f64 = STAGE_GAIN * STAGE_GAIN;

/// One channel of the 4th-order CIC × 8 cascade (two stages). IIR integrators
/// are kept in `i128` to avoid the precision loss of an unbounded `f64` sum.
#[derive(Clone)]
struct Cic {
    s1: i128,
    s2: i128,
    s3: i128,
    s4: i128,
    d1p: i128,
    d2p: i128,
    d3p: i128,
    d4p: i128,
    cnt1: u8,
    t1: i128,
    t2: i128,
    t3: i128,
    t4: i128,
    e1p: i128,
    e2p: i128,
    e3p: i128,
    e4p: i128,
    cnt2: u8,
    x_last: f64,
    y_last: f64,
}

impl Cic {
    fn new() -> Self {
        Cic {
            s1: 0,
            s2: 0,
            s3: 0,
            s4: 0,
            d1p: 0,
            d2p: 0,
            d3p: 0,
            d4p: 0,
            cnt1: 0,
            t1: 0,
            t2: 0,
            t3: 0,
            t4: 0,
            e1p: 0,
            e2p: 0,
            e3p: 0,
            e4p: 0,
            cnt2: 0,
            x_last: 0.0,
            y_last: 0.0,
        }
    }

    fn reset(&mut self) {
        *self = Cic::new();
    }

    /// Feed one 1-bit sample (as ±1) and push any produced PCM samples.
    #[inline]
    fn feed(&mut self, x: i128, out: &mut Vec<f32>) {
        self.s1 += x;
        self.s2 += self.s1;
        self.s3 += self.s2;
        self.s4 += self.s3;
        self.cnt1 += 1;
        if self.cnt1 == CIC_DECIM {
            self.cnt1 = 0;
            let k0 = self.s4;
            let k1 = k0 - self.d1p;
            self.d1p = k0;
            let k2 = k1 - self.d2p;
            self.d2p = k1;
            let k3 = k2 - self.d3p;
            self.d3p = k2;
            let k4 = k3 - self.d4p;
            self.d4p = k3;

            self.t1 += k4;
            self.t2 += self.t1;
            self.t3 += self.t2;
            self.t4 += self.t3;
            self.cnt2 += 1;
            if self.cnt2 == CIC_DECIM {
                self.cnt2 = 0;
                let m0 = self.t4;
                let m1 = m0 - self.e1p;
                self.e1p = m0;
                let m2 = m1 - self.e2p;
                self.e2p = m1;
                let m3 = m2 - self.e3p;
                self.e3p = m2;
                let m4 = m3 - self.e4p;
                self.e4p = m3;

                let y = m4 as f64 / TOTAL_GAIN;
                // 1-pole DC blocker (cutoff ~3 Hz at 44.1 kHz output).
                let yh = y - self.x_last + 0.9995 * self.y_last;
                self.x_last = y;
                self.y_last = yh;
                out.push(yh as f32);
            }
        }
    }
}

/// DSD container header info.
struct DsdHeader {
    dsd_rate: u32,
    channels: usize,
    data_offset: u64,
    audio_bytes: u64,
    planar: bool,
    block_size: usize,
}

fn read_u16_be(buf: &[u8]) -> u16 {
    u16::from_be_bytes([buf[0], buf[1]])
}

fn read_u32_le(buf: &[u8]) -> u32 {
    u32::from_le_bytes(buf[..4].try_into().unwrap())
}

fn read_u32_be(buf: &[u8]) -> u32 {
    u32::from_be_bytes(buf[..4].try_into().unwrap())
}

fn read_u64_le(buf: &[u8]) -> u64 {
    u64::from_le_bytes(buf[..8].try_into().unwrap())
}

fn read_u64_be(buf: &[u8]) -> u64 {
    u64::from_be_bytes(buf[..8].try_into().unwrap())
}

fn dsd_rate_label(rate: u32) -> String {
    let base = if rate % 2822400 == 0 {
        2822400
    } else {
        3072000
    };
    let mult = rate / base;
    match mult {
        1 => "DSD64".to_string(),
        2 => "DSD128".to_string(),
        4 => "DSD256".to_string(),
        8 => "DSD512".to_string(),
        16 => "DSD1024".to_string(),
        _ => format!("DSD ({} Hz)", rate),
    }
}

/// Average bitrate (kbps) for a DSD track, from file size / duration.
fn dsd_bitrate(path: &Path, dsd_rate: u32, channels: usize) -> u32 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let ch = u64::from(channels.max(1) as u32);
    // Duration across all channels: each channel streams `dsd_rate` 1-bit
    // samples/s, so total byte rate = channels * dsd_rate / 8 bytes/s.
    let data_offset = 92u64;
    let audio_bytes = meta.len().saturating_sub(data_offset) as f64;
    let duration = audio_bytes * 8.0 / (f64::from(dsd_rate) * ch as f64);
    if duration <= 0.0 {
        return 0;
    }
    // Average bitrate across all channels, in kbps.
    ((meta.len() as f64 * 8.0) / (duration * 1000.0)) as u32
}

/// Parse a DSF ("DSD Stream File") header. `buf` covers file bytes 0..92.
fn parse_dsf(file: &mut BufReader<File>) -> Result<(DsdHeader, u64), String> {
    let mut buf = [0u8; 92];
    file.read_exact(&mut buf)
        .map_err(|e| format!("Cannot read DSF header: {e}"))?;
    if &buf[0..4] != b"DSD " {
        return Err("Not a DSF file".into());
    }
    // DSD chunk: file size @12, metadata (ID3) offset @20.
    let metadata_offset = read_u64_le(&buf[20..28]);
    // fmt chunk starts at file offset 28.
    if &buf[28..32] != b"fmt " {
        return Err("DSF: missing fmt chunk".into());
    }
    let format_id = read_u32_le(&buf[44..48]);
    let channels = read_u32_le(&buf[52..56]) as usize;
    let dsd_rate = read_u32_le(&buf[56..60]);
    let bits_per_sample = read_u32_le(&buf[60..64]);
    let sample_count = read_u64_le(&buf[64..72]);
    let block_size = read_u32_le(&buf[72..76]) as usize;

    if format_id != 0 {
        return Err("DSF: DST-encoded data not supported".into());
    }
    if bits_per_sample != 1 {
        return Err(format!(
            "DSF: unsupported bits per sample {bits_per_sample}"
        ));
    }
    if dsd_rate == 0 || dsd_rate % 64 != 0 {
        return Err(format!("DSF: unsupported sample rate {dsd_rate}"));
    }
    let per_ch_bytes = sample_count.div_ceil(8);
    let audio_bytes = per_ch_bytes * channels as u64;
    let header = DsdHeader {
        dsd_rate,
        channels,
        data_offset: 92,
        audio_bytes,
        planar: true,
        block_size: if block_size > 0 { block_size } else { 4096 },
    };
    Ok((header, metadata_offset))
}

/// Parse a DSDIFF (DFF) container.
fn parse_dff(file: &mut BufReader<File>) -> Result<(DsdHeader, u64), String> {
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 12];
    file.read_exact(&mut buf)
        .map_err(|e| format!("Cannot read DFF header: {e}"))?;
    if &buf[0..4] != b"FRM8" {
        return Err("Not a DFF file".into());
    }
    // FRM8 is followed by a 4-byte form type (not a chunk header).
    let mut form = [0u8; 4];
    file.read_exact(&mut form)
        .map_err(|_| "DFF: bad form type")?;
    if &form != b"DSD " {
        return Err("DFF: unsupported form type".into());
    }

    let mut dsd_rate = 0u32;
    let mut channels = 0usize;

    let (data_offset, audio_bytes) = 'outer: loop {
        file.read_exact(&mut buf)
            .map_err(|_| "DFF: bad chunk header")?;
        let id = &buf[0..4];
        let size = read_u64_be(&buf[4..12]);
        let padded = size + (size & 1);
        match id {
            b"PROP" => {
                let mut p = [0u8; 4];
                file.read_exact(&mut p).map_err(|_| "DFF: bad PROP")?;
                if &p != b"SND " {
                    file.seek(SeekFrom::Current(padded.saturating_sub(4) as i64))
                        .map_err(|e| format!("DFF: {e}"))?;
                    continue;
                }
                let mut remaining = size - 4;
                while remaining >= 12 {
                    file.read_exact(&mut buf)
                        .map_err(|_| "DFF: bad PROP child")?;
                    let sid = &buf[0..4];
                    let ssize = read_u64_be(&buf[4..12]);
                    let spadded = ssize + (ssize & 1);
                    if remaining < 12 + spadded {
                        break;
                    }
                    let mut data = vec![0u8; ssize as usize];
                    file.read_exact(&mut data)
                        .map_err(|_| "DFF: bad PROP child data")?;
                    match sid {
                        b"FS  " => {
                            if data.len() >= 4 {
                                dsd_rate = read_u32_be(&data);
                            }
                        }
                        b"CHNL" => {
                            // Some writers prepend a 4-byte version field.
                            let v1 = if data.len() >= 2 {
                                usize::from(read_u16_be(&data))
                            } else {
                                0
                            };
                            let v2 = if data.len() >= 6 {
                                usize::from(read_u16_be(&data[4..6]))
                            } else {
                                0
                            };
                            let ch = if (1..=6).contains(&v1) {
                                v1
                            } else if (1..=6).contains(&v2) {
                                v2
                            } else {
                                2
                            };
                            channels = ch;
                        }
                        b"CMPR" => {
                            if data.len() >= 4 && &data[..4] != b"DSD " {
                                return Err("DFF: DST-encoded data not supported".into());
                            }
                        }
                        _ => {}
                    }
                    remaining = remaining.saturating_sub(12 + spadded);
                }
                if remaining > 0 {
                    file.seek(SeekFrom::Current(remaining as i64))
                        .map_err(|e| format!("DFF: {e}"))?;
                }
            }
            b"DSD " => {
                break 'outer (file.stream_position().map_err(|e| e.to_string())?, size);
            }
            _ => {
                if size == 0 {
                    return Err("DFF: zero-sized unknown chunk".into());
                }
                file.seek(SeekFrom::Current(padded as i64))
                    .map_err(|e| format!("DFF: {e}"))?;
            }
        }
    };
    if dsd_rate == 0 || dsd_rate % 64 != 0 {
        return Err(format!("DFF: unsupported sample rate {dsd_rate}"));
    }
    let header = DsdHeader {
        dsd_rate,
        channels: if channels == 0 { 2 } else { channels },
        data_offset,
        audio_bytes,
        planar: false,
        block_size: 1,
    };
    Ok((header, 0))
}

fn decode_utf16(units: &[u16]) -> String {
    let mut preceded_surrogate: Option<u16> = None;
    let mut out = String::new();
    for &u in units {
        if u == 0 {
            continue;
        }
        if (0xD800..0xDC00).contains(&u) {
            preceded_surrogate = Some(u);
            continue;
        }
        if (0xDC00..0xE000).contains(&u) {
            if let Some(hi) = preceded_surrogate.take() {
                let c = 0x10000 + ((hi - 0xD800) as u32) * 0x400 + (u - 0xDC00) as u32;
                out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
            }
            continue;
        }
        out.push(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
    }
    out
}

fn extract_text(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    let enc = data[0];
    let text = &data[1..];
    match enc {
        // UTF-16 with BOM (FF FE = LE, FE FF = BE).
        1 => {
            if text.len() >= 2 && text[0] == 0xFF && text[1] == 0xFE {
                let units: Vec<u16> = text[2..]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                decode_utf16(&units).trim().to_string()
            } else if text.len() >= 2 && text[0] == 0xFE && text[1] == 0xFF {
                let units: Vec<u16> = text[2..]
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                decode_utf16(&units).trim().to_string()
            } else {
                let units: Vec<u16> = text
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                decode_utf16(&units).trim().to_string()
            }
        }
        // UTF-16BE (no BOM).
        2 => {
            let units: Vec<u16> = text
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            decode_utf16(&units).trim().to_string()
        }
        // UTF-8.
        3 => String::from_utf8_lossy(text)
            .trim_matches('\u{0}')
            .trim()
            .to_string(),
        // ISO-8859-1 / extended ASCII.
        _ => text
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect::<String>()
            .trim()
            .to_string(),
    }
}

/// Parse a `"N"` or `"N/M"` pair (track/disc numbers) into (number, total).
fn parse_number_pair_inline(s: &str) -> (u32, u32) {
    let s = s.trim();
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

/// Minimal ID3v2.2/2.3/2.4 and ID3v1 tag reader (TIT2/TPE1/TALB + v2.2 aliases).
fn parse_id3(buf: &[u8]) -> Tags {
    let mut tags = Tags::default();
    if buf.len() >= 10 && &buf[0..3] == b"ID3" {
        let ver = buf[3];
        let s = &buf[6..10];
        let sz = ((s[0] as usize & 0x7f) << 21)
            | ((s[1] as usize & 0x7f) << 14)
            | ((s[2] as usize & 0x7f) << 7)
            | (s[3] as usize & 0x7f);
        let body = &buf[10..(10 + sz).min(buf.len())];
        let mut pos = 0usize;
        while pos + 10 <= body.len() {
            let frame_len_field = if ver == 3 {
                u32::from_be_bytes(body[pos + 4..pos + 8].try_into().unwrap()) as usize
            } else {
                let f4 = &body[pos + 4..pos + 8];
                ((f4[0] as usize & 0x7f) << 21)
                    | ((f4[1] as usize & 0x7f) << 14)
                    | ((f4[2] as usize & 0x7f) << 7)
                    | (f4[3] as usize & 0x7f)
            };
            if frame_len_field == 0 {
                break;
            }
            let id = if ver == 2 {
                &body[pos..pos + 3]
            } else {
                &body[pos..pos + 4]
            };
            if pos + 10 + frame_len_field > body.len() {
                break;
            }
            let data = &body[pos + 10..pos + 10 + frame_len_field];
            match id {
                b"TIT2" | b"TT2" => {
                    if tags.title.is_none() {
                        tags.title = Some(extract_text(data));
                    }
                }
                b"TPE1" | b"TP1" => {
                    if tags.artist.is_none() {
                        tags.artist = Some(extract_text(data));
                    }
                }
                b"TPE2" | b"TP2" => {
                    if tags.album_artist.is_none() {
                        tags.album_artist = Some(extract_text(data));
                    }
                }
                b"TALB" | b"TAL" => {
                    if tags.album.is_none() {
                        tags.album = Some(extract_text(data));
                    }
                }
                b"TCON" | b"TCO" => {
                    if tags.genre.is_none() {
                        tags.genre = Some(extract_text(data));
                    }
                }
                b"TDRC" | b"TYER" | b"TORY" => {
                    if tags.year.is_none() {
                        tags.year = Some(extract_text(data));
                    }
                }
                b"TRCK" | b"TRK" => {
                    let text = extract_text(data);
                    let (n, t) = parse_number_pair_inline(&text);
                    if tags.track_number == 0 {
                        tags.track_number = n;
                    }
                    tags.track_total = tags.track_total.max(t);
                }
                b"TPOS" | b"TPA" => {
                    let text = extract_text(data);
                    let (n, t) = parse_number_pair_inline(&text);
                    if tags.disc_number == 0 {
                        tags.disc_number = n;
                    }
                    tags.disc_total = tags.disc_total.max(t);
                }
                _ => {}
            }
            pos += 10 + frame_len_field;
        }
    } else if buf.len() >= 128 && &buf[0..3] == b"TAG" {
        let title = String::from_utf8_lossy(&buf[3..33]).trim().to_string();
        let artist = String::from_utf8_lossy(&buf[33..63]).trim().to_string();
        let album = String::from_utf8_lossy(&buf[63..93]).trim().to_string();
        if tags.title.is_none() && !title.is_empty() {
            tags.title = Some(title);
        }
        if !artist.is_empty() {
            tags.artist = Some(artist);
        }
        if !album.is_empty() {
            tags.album = Some(album);
        }
    }
    tags
}

/// Read DSD container tags: ID3v2 at the DSF metadata offset (if any), else
/// scan the file tail for an ID3 chunk.
fn read_container_tags(path: &Path, metadata_offset: u64) -> Tags {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Tags::default(),
    };
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    if metadata_offset != 0 && metadata_offset + 10 <= len {
        let mut buf = vec![0u8; (len - metadata_offset) as usize];
        if f.seek(SeekFrom::Start(metadata_offset)).is_ok() && f.read_exact(&mut buf).is_ok() {
            let tags = parse_id3(&buf);
            if tags.title.is_some() || tags.artist.is_some() {
                return tags;
            }
        }
    }

    let tail_len = len.min(1 << 20);
    let mut tail = vec![0u8; tail_len as usize];
    if f.seek(SeekFrom::Start(len - tail_len)).is_err() || f.read_exact(&mut tail).is_err() {
        return Tags::default();
    }
    if tail.len() >= 128 && &tail[tail.len() - 128..tail.len() - 125] == b"TAG" {
        return parse_id3(&tail[tail.len() - 128..]);
    }
    for i in (0..tail.len().saturating_sub(10)).rev() {
        if &tail[i..i + 3] == b"ID3" {
            return parse_id3(&tail[i..]);
        }
    }
    Tags::default()
}

/// Lightweight metadata probe for DSD files (tags + duration), without
/// building a full decoder. Used by the background tag filler.
pub fn probe_dsd(path: &Path) -> crate::meta::FileMeta {
    let mut file = BufReader::new(match File::open(path) {
        Ok(f) => f,
        Err(_) => return crate::meta::FileMeta::default(),
    });
    let (header, meta_offset) = match path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("dsf") => match parse_dsf(&mut file) {
            Ok(h) => h,
            Err(_) => return crate::meta::FileMeta::default(),
        },
        Some(e) if e.eq_ignore_ascii_case("dff") => match parse_dff(&mut file) {
            Ok(h) => h,
            Err(_) => return crate::meta::FileMeta::default(),
        },
        _ => return crate::meta::FileMeta::default(),
    };
    let tags = read_container_tags(path, meta_offset);
    let per_ch_bytes = header.audio_bytes / header.channels.max(1) as u64;
    let pcm_frames = per_ch_bytes / 8; // 64 DSD samples/channel == 8 bytes
    let pcm_rate = (header.dsd_rate / 64).max(1);
    let duration = Some(pcm_frames as f64 / pcm_rate as f64);
    let bitrate = dsd_bitrate(path, header.dsd_rate, header.channels);
    crate::meta::FileMeta {
        tags,
        duration,
        // Native DSD sample rate for display (e.g. DSD64 = 352800, DSD128 =
        // 705600, DSD256 = 1411200). Computed as dsd_rate / 8 (each of the
        // 64 decimation groups of 8 bits represents one displayed sample).
        sample_rate: (header.dsd_rate / 8).max(1),
        channels: header.channels as u32,
        bitrate,
        bits: None,
        dsd_label: Some(dsd_rate_label(header.dsd_rate).to_lowercase()),
    }
}

pub struct DsdDecoder {
    file: BufReader<File>,
    header: DsdHeader,
    pcm_rate: u32,
    bytes_remaining: u64,
    cic: Vec<Cic>,
    raw: Vec<u8>,
    pcm: Vec<f32>,
    pcm_frames: usize,
    info: TrackInfo,
    eof: bool,
}

const RAW_GROUP: usize = 4096;

impl DsdDecoder {
    pub fn open(path: &Path) -> Result<DsdDecoder, String> {
        let mut file =
            BufReader::new(File::open(path).map_err(|e| format!("Cannot open file: {e}"))?);
        let (header, meta_offset) = match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("dsf") => parse_dsf(&mut file)?,
            Some(e) if e.eq_ignore_ascii_case("dff") => parse_dff(&mut file)?,
            _ => return Err("Not a DSD file".into()),
        };
        let pcm_rate = header.dsd_rate / 64;
        let tags = read_container_tags(path, meta_offset);
        // One PCM frame per channel == 64 DSD samples == 8 bytes of audio.
        let per_ch_bytes = header.audio_bytes / header.channels as u64;
        let pcm_frames_total = per_ch_bytes / 8;
        let bitrate = dsd_bitrate(path, header.dsd_rate, header.channels);
        let info = TrackInfo {
            sample_rate: pcm_rate,
            channels: header.channels,
            num_frames: Some(pcm_frames_total as u64),
            format_name: dsd_rate_label(header.dsd_rate),
            bitrate,
            bits: None,
            tags,
        };
        file.seek(SeekFrom::Start(header.data_offset))
            .map_err(|e| format!("DSD: {e}"))?;
        let ch_count = header.channels;
        let raw_len = (header.block_size * ch_count + 64).max(RAW_GROUP * ch_count);
        Ok(DsdDecoder {
            file,
            pcm_rate,
            bytes_remaining: header.audio_bytes,
            header,
            cic: (0..ch_count).map(|_| Cic::new()).collect(),
            raw: vec![0u8; raw_len],
            pcm: Vec::with_capacity((RAW_GROUP / 8) * ch_count),
            pcm_frames: 0,
            info,
            eof: false,
        })
    }

    /// Decode one raw group into `self.pcm` (interleaved). Returns frames per
    /// channel produced, or 0 when all audio bytes are consumed.
    fn decode_group(&mut self) -> usize {
        if self.bytes_remaining == 0 {
            return 0;
        }
        let ch = self.header.channels;
        // Fresh per-channel output buffers.
        let mut per_ch = vec![Vec::with_capacity(RAW_GROUP / 8); ch];

        if self.header.planar {
            // DSF: blocks are stored per channel, one `block_size` block per
            // channel per period: [ch0 block][ch1 block]... Read whole periods
            // so each channel always gets its own full block.
            let period = self.header.block_size * ch;
            let take = (self.bytes_remaining as usize).min(period);
            if take == 0 {
                self.bytes_remaining = 0;
                return 0;
            }
            let _read = read_all(&mut self.file, &mut self.raw[..take]);
            let valid_per_ch = take / ch;
            for chn in 0..ch {
                let base = chn * valid_per_ch;
                let o = &mut per_ch[chn];
                for &byte in &self.raw[base..base + valid_per_ch] {
                    // LSB-first: bit 0 is the earliest sample.
                    for b in 0..8 {
                        let x = if (byte >> b) & 1 == 1 { 1i128 } else { -1i128 };
                        self.cic[chn].feed(x, o);
                    }
                }
            }
            self.bytes_remaining -= take as u64;
        } else {
            // DFF: bytes interleaved by channel.
            let positions = (self.bytes_remaining as usize / ch).min(RAW_GROUP);
            if positions == 0 {
                self.bytes_remaining = 0;
                return 0;
            }
            let take = positions * ch;
            let read = read_all(&mut self.file, &mut self.raw[..take]);
            let n = read / ch;
            for pos in 0..n {
                for chn in 0..ch {
                    let byte = self.raw[pos * ch + chn];
                    // MSB-first: bit 7 is the earliest sample.
                    let o = &mut per_ch[chn];
                    for b in 0..8 {
                        let x = if (byte >> (7 - b)) & 1 == 1 {
                            1i128
                        } else {
                            -1i128
                        };
                        self.cic[chn].feed(x, o);
                    }
                }
            }
            self.bytes_remaining -= take as u64;
        }

        let frames = per_ch[0].len();
        self.pcm.clear();
        self.pcm.reserve(frames * ch);
        for f in 0..frames {
            for chn in 0..ch {
                self.pcm.push(per_ch[chn][f]);
            }
        }
        self.pcm_frames = frames;
        frames
    }
}

fn read_all<R: Read>(r: &mut R, buf: &mut [u8]) -> usize {
    let mut total = 0usize;
    while total < buf.len() {
        match r.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    total
}

impl AudioSource for DsdDecoder {
    fn next_frames(&mut self) -> Option<&[f32]> {
        if self.pcm_frames == 0 {
            if self.decode_group() == 0 {
                self.eof = true;
                return None;
            }
        }
        let ch = self.header.channels;
        let n = self.pcm_frames * ch;
        self.pcm_frames = 0;
        Some(&self.pcm[..n])
    }

    fn seek(&mut self, secs: f64) -> Result<(), String> {
        let secs = secs.max(0.0);
        let ch = self.header.channels as u64;
        let target_frames = (secs * self.pcm_rate as f64)
            .min(self.info.num_frames.unwrap_or(u64::MAX) as f64)
            as u64;
        let per_ch_byte = target_frames * 8;
        // DSF (planar) stores one `block_size` block per channel per period;
        // align the byte position so each channel starts at its own block.
        let per_ch_aligned = if self.header.planar {
            (per_ch_byte / self.header.block_size as u64) * self.header.block_size as u64
        } else {
            per_ch_byte
        };
        let start_byte = self.header.data_offset + per_ch_aligned * ch;
        self.file
            .seek(SeekFrom::Start(start_byte))
            .map_err(|e| format!("DSD seek failed: {e}"))?;
        self.bytes_remaining = self.header.audio_bytes.saturating_sub(per_ch_aligned * ch);
        for c in self.cic.iter_mut() {
            c.reset();
        }
        self.pcm_frames = 0;
        self.pcm.clear();
        self.eof = false;
        Ok(())
    }

    fn info(&self) -> &TrackInfo {
        &self.info
    }

    fn eof(&self) -> bool {
        self.eof
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn open_from_env(key: &str) -> Option<PathBuf> {
        let var = std::env::var_os(key);
        match var {
            Some(p) => Some(PathBuf::from(p)),
            None => {
                eprintln!("skipped: {key} not set");
                None
            }
        }
    }

    fn assert_common(dec: &DsdDecoder) {
        assert!(matches!(
            dec.info.sample_rate,
            44100 | 48000 | 88200 | 96000 | 176400 | 192000
        ));
        assert_eq!(dec.info.channels, 2);
        assert!(dec.info.num_frames.unwrap_or(0) > 0, "expected frame count");
        assert!(dec.info.format_name.starts_with("DSD"));
    }

    fn decode_second(dec: &mut DsdDecoder, seconds: usize) -> usize {
        let ch = dec.info.channels;
        let rate = dec.info.sample_rate as usize;
        let target = rate * seconds;
        let mut total = 0usize;
        let mut finite = true;
        let mut nzi = 0usize;
        let mut peak = 0.0f32;
        for _ in 0..100_000 {
            match dec.next_frames() {
                Some(buf) => {
                    total += buf.len() / ch;
                    finite &= buf.iter().all(|s| s.is_finite());
                    nzi += buf.iter().take(ch * 16).filter(|s| s.abs() > 1e-6).count();
                    peak = peak.max(buf.iter().fold(0.0f32, |a, &s| a.max(s.abs())));
                    if total >= target {
                        break;
                    }
                }
                None => break,
            }
        }
        assert!(total > 0, "no audio produced");
        assert!(finite, "non-finite samples");
        assert!(nzi > 0, "signal is silent");
        assert!(peak > 1e-4 && peak < 4.0, "implausible peak {peak}");
        total
    }

    #[test]
    fn dsf_decode_headless() {
        let Some(path) = open_from_env("MUSIC_DSD_TEST_FILE") else {
            return;
        };
        let mut dec = DsdDecoder::open(&path).expect("open DSF");
        assert_common(&dec);
        decode_second(&mut dec, 1);
        dec.seek(30.0).expect("seek");
        let mut saw = false;
        for _ in 0..100_000 {
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
    fn dff_decode_headless() {
        let Some(path) = open_from_env("MUSIC_DFF_TEST_FILE") else {
            return;
        };
        let mut dec = DsdDecoder::open(&path).expect("open DFF");
        assert_common(&dec);
        decode_second(&mut dec, 1);
        dec.seek(30.0).expect("seek");
        let mut saw = false;
        for _ in 0..100_000 {
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
    fn id3_utf16_tags() {
        // Regression: UTF-16LE with BOM (enc=1, FF FE) must decode as text,
        // not as a byte-swapped garbage (the "squares" bug).
        let mut buf = Vec::new();
        let frames: &[(&str, &str)] = &[
            ("TPE1", "Genesis"),
            ("TIT2", "Invisible Touch"),
            ("TALB", "Invisible Touch"),
        ];
        let mut body_bytes = 0usize;
        for (id, text) in frames {
            let mut data = vec![0xFF, 0xFE]; // BOM LE
            for &c in text.as_bytes() {
                data.extend_from_slice(&(c as u16).to_le_bytes());
            }
            let fsize = data.len() as u32 + 1;
            body_bytes += 10 + fsize as usize;
            buf.extend_from_slice(id.as_bytes());
            buf.extend_from_slice(&fsize.to_be_bytes());
            buf.extend_from_slice(&[0, 0]);
            buf.push(1); // encoding: UTF-16 with BOM
            buf.extend_from_slice(&data);
        }
        let tag = {
            let mut t = b"ID3\x03\x00\x00".to_vec();
            let sz = body_bytes as u32;
            t.extend_from_slice(&[
                ((sz >> 21) & 0x7f) as u8,
                ((sz >> 14) & 0x7f) as u8,
                ((sz >> 7) & 0x7f) as u8,
                (sz & 0x7f) as u8,
            ]);
            t.extend_from_slice(&buf);
            t
        };
        let tags = parse_id3(&tag);
        assert_eq!(tags.artist.as_deref(), Some("Genesis"), "{:?}", tags);
        assert_eq!(tags.title.as_deref(), Some("Invisible Touch"), "{:?}", tags);
        assert_eq!(tags.album.as_deref(), Some("Invisible Touch"), "{:?}", tags);
    }
}
