use std::path::Path;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::decoder::{AudioSource, Decoder, TrackInfo};
use super::dsd::DsdDecoder;
use super::output::{build_stream, select_output, Resampler};

/// Shared state accessed both by the UI thread and the audio callback.
pub struct PlaybackCore {
    pub decoder: Option<Box<dyn AudioSource>>,
    pub resampler: Option<Resampler>,
    pub volume: f32,
    pub muted: bool,
    pub playing: bool,
    pub finished: bool,
    /// True when the track reached its natural end-of-stream (as opposed to a
    /// manual Stop). Drives playlist auto-advance only.
    pub natural_end: bool,
    pub pos_secs: f64,
    pub out_rate: u32,
    pub out_ch: usize,
    scratch: Vec<f32>,
}

impl PlaybackCore {
    pub(crate) fn new() -> Self {
        Self {
            decoder: None,
            resampler: None,
            volume: 0.8,
            muted: false,
            playing: false,
            finished: true,
            natural_end: false,
            pos_secs: 0.0,
            out_rate: 44100,
            out_ch: 2,
            scratch: Vec::with_capacity(8192),
        }
    }

    fn fill(&mut self, out: &mut [f32]) -> usize {
        if self.decoder.is_none() {
            return 0;
        }
        let out_ch = self.out_ch;
        let mut written_samples = 0usize;
        loop {
            if written_samples >= out.len() {
                break;
            }
            let remaining_frames = (out.len() - written_samples) / out_ch;
            if remaining_frames == 0 {
                break;
            }
            let eof = self.decoder.as_ref().map(|d| d.eof()).unwrap_or(true);
            match &mut self.resampler {
                Some(res) => {
                    // Keep at least one source frame buffered for interpolation.
                    let mut guard = 0;
                    while res.buffered_frames() < 2 && !eof && guard < 1024 {
                        guard += 1;
                        let Some(s) = self.decoder.as_mut().and_then(|d| d.next_frames()) else {
                            break;
                        };
                        res.push(s);
                    }
                    let produced_frames =
                        res.pull(&mut out[written_samples..], remaining_frames, eof);
                    if produced_frames == 0 {
                        break;
                    }
                    written_samples += produced_frames * out_ch;
                }
                None => break,
            }
        }
        written_samples
    }

    fn scratch_f32(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.scratch)
    }

    fn scratch_release(&mut self, buf: Vec<f32>) {
        let mut buf = buf;
        buf.clear();
        if buf.capacity() > 128 * 1024 {
            buf.shrink_to(64 * 1024);
        }
        self.scratch = buf;
    }

    fn duration_secs(&self) -> Option<f64> {
        self.decoder.as_ref().and_then(|d| d.duration_secs())
    }
}

/// Owns audio playback: the shared [`PlaybackCore`] (decoder + resampler +
/// transport state, also read by the cpal callback) and the running output
/// stream. All control happens on the caller's thread; only the cpal
/// callback touches the core concurrently.
pub struct Player {
    pub core: Arc<Mutex<PlaybackCore>>,
    pub stream: Option<cpal::Stream>,
    pub device_desc: String,
    pub last_error: Option<String>,
    preferred_device: Option<String>,
}

impl Player {
    pub fn new() -> Self {
        let host = cpal::default_host();
        let device_desc = host
            .default_output_device()
            .and_then(|d| d.description().ok())
            .map(|d| d.name().to_string())
            .unwrap_or_else(|| "none".to_string());
        Self {
            core: Arc::new(Mutex::new(PlaybackCore::new())),
            stream: None,
            device_desc,
            last_error: None,
            preferred_device: None,
        }
    }

    pub fn set_preferred_device(&mut self, name: String) {
        self.preferred_device = Some(name);
    }

    /// Change the output device. When `path` is given the current track is
    /// reopened on the new device, resumed near `pos` and set playing.
    pub fn set_device(
        &mut self,
        name: String,
        path: Option<&Path>,
        pos: f64,
    ) -> Result<(), String> {
        let was_playing = self.is_playing();
        self.preferred_device = Some(name.clone());
        if let Some(path) = path {
            self.open(path)?;
            if pos > 0.0 {
                self.seek(pos);
            }
            if was_playing || pos > 0.0 {
                self.play();
            }
        }
        Ok(())
    }

    /// Open `path` for playback: pick an output device/format via
    /// [`select_output`](crate::audio::output::select_output), build the stream
    /// and reset transport state. Returns the track's format info.
    /// Errors (unsupported file, no device) are returned as strings.
    pub fn open(&mut self, path: &Path) -> Result<TrackInfo, String> {
        // Open and probe outside the audio-thread lock so the callback never
        // blocks on slow I/O.
        let src: Box<dyn AudioSource> = match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("dsf") || e.eq_ignore_ascii_case("dff") => {
                Box::new(DsdDecoder::open(path)?)
            }
            _ => Box::new(Decoder::open(path)?),
        };
        let (src_rate, src_ch) = (src.info().sample_rate, src.info().channels);
        let info = src.info().clone();

        // Rebuild the output stream for the current track parameters.
        self.stream = None;
        let spec = select_output(src_rate, src_ch, self.preferred_device.as_deref())?;
        let out_rate = spec.config.sample_rate;
        let out_ch = spec.config.channels as usize;
        self.device_desc = spec.device_name.clone();

        {
            let mut core = match self.core.lock() {
                Ok(c) => c,
                Err(_) => return Err("audio core poisoned; cannot open track".into()),
            };
            core.resampler = Some(Resampler::new(src_rate, out_rate, src_ch, out_ch));
            core.out_rate = out_rate;
            core.out_ch = out_ch;
            core.decoder = Some(src);
            core.pos_secs = 0.0;
            core.playing = false;
            core.finished = false;
            core.natural_end = false;
            core.scratch.clear();
        }

        let stream = build_stream(&spec, self.core.clone(), None)?;
        if let Err(e) = stream.play() {
            self.last_error = Some(format!("Cannot start stream: {e}"));
        }
        self.stream = Some(stream);
        self.last_error = None;

        Ok(info)
    }

    /// Start/resume playback. A finished track is rewound and replayed.
    pub fn play(&mut self) {
        let Ok(mut core) = self.core.lock() else {
            self.last_error = Some("audio core busy/poisoned".into());
            return;
        };
        if core.decoder.is_none() {
            return;
        }
        if core.finished {
            core.pos_secs = 0.0;
            if let Some(dec) = &mut core.decoder {
                let _ = dec.seek(0.0);
            }
            if let Some(res) = &mut core.resampler {
                res.reset();
            }
            core.finished = false;
        }
        core.natural_end = false;
        core.playing = true;
    }

    /// Pause/resume the current track (rewinds if it had finished).
    pub fn toggle(&mut self) {
        let Ok(mut core) = self.core.lock() else {
            self.last_error = Some("audio core busy/poisoned".into());
            return;
        };
        if core.decoder.is_none() {
            return;
        }
        if core.playing {
            core.playing = false;
        } else {
            if core.finished {
                core.pos_secs = 0.0;
                if let Some(dec) = &mut core.decoder {
                    let _ = dec.seek(0.0);
                }
                if let Some(res) = &mut core.resampler {
                    res.reset();
                }
                core.finished = false;
            }
            core.natural_end = false;
            core.playing = true;
        }
    }

    /// Stop playback, mark the track as finished and rewind to the start.
    pub fn stop(&mut self) {
        let Ok(mut core) = self.core.lock() else { return };
        core.playing = false;
        core.finished = true;
        core.natural_end = false;
        core.pos_secs = 0.0;
        if let Some(dec) = &mut core.decoder {
            let _ = dec.seek(0.0);
        }
        if let Some(res) = &mut core.resampler {
            res.reset();
        }
    }

    /// Seek to `secs` (clamped to >= 0). Resets the resampler so the new
    /// position is played from the decoder, not computed from stale buffers.
    pub fn seek(&mut self, secs: f64) {
        let Ok(mut core) = self.core.lock() else { return };
        if let Some(dec) = &mut core.decoder {
            if dec.seek(secs).is_ok() {
                if let Some(res) = &mut core.resampler {
                    res.reset();
                }
                core.pos_secs = secs.max(0.0);
                core.finished = false;
                core.natural_end = false;
            }
        }
    }

    /// Set volume, clamped to [0, 1]. Applied inside the audio callback.
    pub fn set_volume(&mut self, v: f32) {
        if let Ok(mut core) = self.core.lock() {
            core.volume = v.clamp(0.0, 1.0);
        }
    }

    /// Current volume in [0, 1] (fallback 0.8 while the core is busy).
    pub fn volume(&self) -> f32 {
        self.core.lock().map(|c| c.volume).unwrap_or(0.8)
    }

    /// Mute/unmute without changing the volume.
    pub fn set_muted(&mut self, m: bool) {
        if let Ok(mut core) = self.core.lock() {
            core.muted = m;
        }
    }

    /// Mute state.
    pub fn muted(&self) -> bool {
        self.core.lock().map(|c| c.muted).unwrap_or(false)
    }

    /// Toggle mute.
    pub fn toggle_mute(&mut self) {
        if let Ok(mut core) = self.core.lock() {
            core.muted = !core.muted;
        }
    }

    /// Whether the core is actively decoding (not paused/stopped).
    pub fn is_playing(&self) -> bool {
        self.core.lock().map(|c| c.playing).unwrap_or(false)
    }

    /// True when the current track played to its natural end (EOF).
    pub fn ended(&self) -> bool {
        self.core.lock().map(|c| c.natural_end).unwrap_or(false)
    }

    /// Clear the natural-end flag (e.g. after the playlist handled it).
    pub fn clear_end(&mut self) {
        if let Ok(mut core) = self.core.lock() {
            core.natural_end = false;
        }
    }

    /// Return a snapshot for the UI: (playing, pos, duration).
    pub fn snapshot(&self) -> (bool, f64, Option<f64>) {
        let core = match self.core.lock() {
            Ok(c) => c,
            Err(_) => return (false, 0.0, None),
        };
        (core.playing, core.pos_secs, core.duration_secs())
    }
}

fn effective_volume(core: &PlaybackCore) -> f32 {
    if core.muted {
        0.0
    } else {
        core.volume
    }
}

// ---- Audio callback entry points (called from the cpal audio thread). ----
//
// These run on the real-time audio thread and must never block. They use
// `try_lock()`; if the UI thread holds the lock (e.g. during a seek or track
// open), the callback emits silence for that buffer instead of blocking.

/// cpal callback for f32 output: pulls up to `data.len()` frames from the
/// decoder/resampler, applies volume, returns silence on lock contention.
pub fn audio_callback_f32(core: &Arc<Mutex<PlaybackCore>>, data: &mut [f32]) {
    let Ok(mut c) = core.try_lock() else {
        data.fill(0.0);
        return;
    };
    if !c.playing {
        data.fill(0.0);
        return;
    }
    let vol = effective_volume(&c);
    let produced = c.fill(data);
    c.pos_secs += produced as f64 / c.out_rate as f64 / c.out_ch as f64;
    for s in data.iter_mut().take(produced) {
        *s *= vol;
    }
    if produced < data.len() {
        data[produced..].fill(0.0);
        if c.decoder.as_ref().map(|d| d.eof()).unwrap_or(false) {
            c.playing = false;
            c.finished = true;
            c.natural_end = true;
        }
    }
}

/// cpal callback for i16 output (see [`audio_callback_f32`]).
pub fn audio_callback_i16(core: &Arc<Mutex<PlaybackCore>>, data: &mut [i16]) {
    let Ok(mut c) = core.try_lock() else {
        data.fill(0);
        return;
    };
    if !c.playing {
        data.fill(0);
        return;
    }
    let vol = effective_volume(&c);
    let out_ch = c.out_ch;
    let mut tmp = c.scratch_f32();
    tmp.resize(data.len(), 0.0);
    let produced = c.fill(&mut tmp);
    c.pos_secs += produced as f64 / c.out_rate as f64 / out_ch as f64;
    for (dst, src) in data.iter_mut().zip(tmp.iter()).take(produced) {
        *dst = (src.clamp(-1.0, 1.0) * vol * 32767.0) as i16;
    }
    for s in data.iter_mut().skip(produced) {
        *s = 0;
    }
    if produced < data.len() && c.decoder.as_ref().map(|d| d.eof()).unwrap_or(false) {
        c.playing = false;
        c.finished = true;
        c.natural_end = true;
    }
    c.scratch_release(tmp);
}

/// cpal callback for u8 output (silence = 128, samples centered at 0.5).
pub fn audio_callback_u8(core: &Arc<Mutex<PlaybackCore>>, data: &mut [u8]) {
    let Ok(mut c) = core.try_lock() else {
        data.fill(128);
        return;
    };
    if !c.playing {
        data.fill(128);
        return;
    }
    let vol = effective_volume(&c);
    let out_ch = c.out_ch;
    let mut tmp = c.scratch_f32();
    tmp.resize(data.len(), 0.0);
    let produced = c.fill(&mut tmp);
    c.pos_secs += produced as f64 / c.out_rate as f64 / out_ch as f64;
    for (dst, src) in data.iter_mut().zip(tmp.iter()).take(produced) {
        *dst = ((src.clamp(-1.0, 1.0) * vol * 0.5 + 0.5) * 255.0) as u8;
    }
    for s in data.iter_mut().skip(produced) {
        *s = 128;
    }
    if produced < data.len() && c.decoder.as_ref().map(|d| d.eof()).unwrap_or(false) {
        c.playing = false;
        c.finished = true;
        c.natural_end = true;
    }
    c.scratch_release(tmp);
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

// Keep stream alive (no-op guard used by the main loop).
#[allow(dead_code)]
/// Keep the stream object alive for its intended lifetime (used by the
/// startup probe, which must hold the stream until the device is checked).
pub fn keep_alive(_s: &cpal::Stream) {}
