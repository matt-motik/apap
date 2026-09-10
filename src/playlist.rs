use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use walkdir::WalkDir;

use crate::settings::ColumnId;

pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "wav", "aiff", "aif", "alac", "m4a", "m4b", "mp4", "mp3", "mp2", "ogg", "oga", "opus",
    "aac", "dsf", "dff",
];

pub fn is_supported_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Default)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub duration: Option<f64>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    /// Track number in its album (0 when unknown).
    pub track_number: u32,
    /// Total track count in its album (0 when unknown).
    pub track_total: u32,
    /// Disc number (0 when unknown).
    pub disc: u32,
    /// Total disc count (0 when unknown).
    pub disc_total: u32,
    /// Number of audio channels.
    pub channels: u32,
    /// Year (e.g. "2019"), empty when unknown.
    pub year: String,
    /// Container/format name (e.g. "FLAC", "DSF", "MP3").
    pub format: String,
    /// Average bitrate in kbps.
    pub bitrate: u32,
    /// Display string for the "Bit Depth" column, e.g. "24 bit", "dsd64".
    pub bit_depth: String,
    /// Sample rate in Hz.
    pub sample_rate: u32,
}

pub enum ScanMsg {
    Batch(Vec<Track>),
    Done(usize),
}

/// Number of tracks sent per `ScanMsg::Batch` while scanning a folder. Larger
/// batches reduce channel overhead; smaller ones keep the UI responsive.
pub const SCAN_BATCH_SIZE: usize = 200;

/// Probe a mixed list of files/directories (from "Add Files"/"Add Folder") off
/// the UI thread. Directories are walked recursively. Results are streamed as
/// [`ScanMsg::Batch`]es (so the UI can show progress) and finished with
/// [`ScanMsg::Done`].
pub fn probe_paths(paths: Vec<PathBuf>, tx: Sender<ScanMsg>) {
    let mut batch: Vec<Track> = Vec::new();
    let mut total = 0usize;
    for p in paths {
        if p.is_dir() {
            for entry in WalkDir::new(&p).follow_links(true).into_iter().flatten() {
                if entry.file_type().is_file() && is_supported_audio(entry.path()) {
                    batch.push(track_for_path(entry.path()));
                    total += 1;
                    if batch.len() >= SCAN_BATCH_SIZE
                        && tx.send(ScanMsg::Batch(std::mem::take(&mut batch))).is_err()
                    {
                        return;
                    }
                }
            }
        } else if is_supported_audio(&p) {
            batch.push(track_for_path(&p));
            total += 1;
            if batch.len() >= SCAN_BATCH_SIZE
                && tx.send(ScanMsg::Batch(std::mem::take(&mut batch))).is_err()
            {
                return;
            }
        }
    }
    if !batch.is_empty() && tx.send(ScanMsg::Batch(batch)).is_err() {
        return;
    }
    let _ = tx.send(ScanMsg::Done(total));
}

pub fn track_for_path(path: &Path) -> Track {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let meta = crate::meta::probe_file(path);
    let format = crate::meta::format_name_for_ext(&ext(path)).to_string();
    Track {
        path: path.to_path_buf(),
        title: meta.tags.title.clone().unwrap_or(stem),
        duration: meta.duration,
        artist: meta.tags.artist.clone(),
        album: meta.tags.album.clone(),
        genre: meta.tags.genre.clone(),
        track_number: meta.tags.track_number,
        track_total: meta.tags.track_total,
        disc: meta.tags.disc_number,
        disc_total: meta.tags.disc_total,
        channels: meta.channels,
        year: meta.tags.year.clone().unwrap_or_default(),
        format,
        bitrate: meta.bitrate,
        bit_depth: meta.bit_depth_string(),
        sample_rate: meta.sample_rate,
    }
}

/// Lowercased extension of a path ("" when none).
fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
}

pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return String::from("--:--");
    }
    let total = secs.round() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn get_duration_string(duration: Option<f64>) -> String {
    duration
        .map(format_duration)
        .unwrap_or_else(|| "--:--".into())
}

/// Linear (non-shuffle) navigation destination. `direction` is +1 for next,
/// -1 for prev. Repeat::All wraps; Off/One advance without wrapping (manual
/// navigation under Repeat One still moves to the next track).
pub fn advance_index(
    current: Option<usize>,
    direction: i32,
    n: usize,
    repeat: crate::settings::RepeatMode,
) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let cur = current.unwrap_or(0).min(n - 1) as i64;
    match repeat {
        crate::settings::RepeatMode::All => {
            Some((cur + direction as i64).rem_euclid(n as i64) as usize)
        }
        _ => {
            let next = cur + direction as i64;
            if next < 0 || next >= n as i64 {
                None
            } else {
                Some(next as usize)
            }
        }
    }
}

/// Shuffle navigation along a permuted `order`. Returns the destination
/// playlist index and the new position within `order`, or None when the move
/// is not allowed (end/start without Repeat::All).
pub fn advance_shuffle(
    order: &[usize],
    pos: usize,
    direction: i32,
    repeat: crate::settings::RepeatMode,
) -> Option<(usize, usize)> {
    let n = order.len();
    if n == 0 {
        return None;
    }
    let pos = pos.min(n - 1) as i64;
    match repeat {
        crate::settings::RepeatMode::All => {
            let np = (pos + direction as i64).rem_euclid(n as i64) as usize;
            Some((order[np], np))
        }
        _ => {
            let np = pos + direction as i64;
            if np < 0 || np >= n as i64 {
                None
            } else {
                Some((order[np as usize], np as usize))
            }
        }
    }
}

/// Load a plain M3U playlist (one absolute path per line).
pub fn load_track_list(path: &Path) -> Vec<Track> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';'))
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && is_supported_audio(p))
        .filter(|p| seen.insert(p.to_path_buf()))
        .map(|p| track_for_path(&p))
        .collect()
}

/// Save the playlist as a plain M3U file (atomically via a temp file).
pub fn save_track_list(path: &Path, tracks: &[Track]) -> bool {
    let mut out = String::new();
    for t in tracks {
        out.push_str(&t.path.to_string_lossy());
        out.push('\n');
    }
    let parent = path.parent().unwrap_or(path);
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let tmp = path.with_extension("m3u.tmp");
    if std::fs::write(&tmp, out).is_err() {
        return false;
    }
    std::fs::rename(&tmp, path).is_ok()
}

/// Display text for a track cell in a given column ("", "0", "24 bit", ...).
pub fn sort_rows_text(track: &Track, col: ColumnId) -> String {
    match col {
        ColumnId::NowPlaying => String::new(),
        ColumnId::TrackNumber => {
            if track.track_number > 0 {
                track.track_number.to_string()
            } else {
                String::new()
            }
        }
        ColumnId::Title => track.title.clone(),
        ColumnId::Artist => track.artist.clone().unwrap_or_default(),
        ColumnId::Album => track.album.clone().unwrap_or_default(),
        ColumnId::Genre => track.genre.clone().unwrap_or_default(),
        ColumnId::Year => track.year.clone(),
        ColumnId::Format => track.format.clone(),
        ColumnId::Bitrate => {
            if track.bitrate > 0 {
                format!("{} kbps", track.bitrate)
            } else {
                String::new()
            }
        }
        ColumnId::BitDepth => track.bit_depth.clone(),
        ColumnId::SampleRate => {
            if track.sample_rate > 0 {
                format!("{} Hz", track.sample_rate)
            } else {
                String::new()
            }
        }
        ColumnId::Duration => get_duration_string(track.duration),
        ColumnId::FileName => track
            .path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ColumnId::FilePath => track.path.to_string_lossy().into_owned(),
    }
}

fn opt_str(s: Option<&str>) -> &str {
    s.filter(|x| !x.is_empty()).unwrap_or("")
}

/// Compare two tracks for sorting by a column (ascending).
pub fn sort_rows_compare(a: &Track, b: &Track, col: ColumnId) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match col {
        ColumnId::TrackNumber => a.track_number.cmp(&b.track_number),
        ColumnId::Title => a.title.cmp(&b.title),
        ColumnId::Artist => opt_str(a.artist.as_deref()).cmp(opt_str(b.artist.as_deref())),
        ColumnId::Album => opt_str(a.album.as_deref()).cmp(opt_str(b.album.as_deref())),
        ColumnId::Genre => opt_str(a.genre.as_deref()).cmp(opt_str(b.genre.as_deref())),
        ColumnId::Year => a.year.cmp(&b.year),
        ColumnId::Format => a.format.cmp(&b.format),
        ColumnId::Bitrate => a.bitrate.cmp(&b.bitrate),
        ColumnId::BitDepth => a.bit_depth.cmp(&b.bit_depth),
        ColumnId::SampleRate => a.sample_rate.cmp(&b.sample_rate),
        ColumnId::Duration => a
            .duration
            .partial_cmp(&b.duration)
            .unwrap_or(Ordering::Equal),
        ColumnId::FileName => {
            let fa = a
                .path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let fb = b
                .path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            fa.cmp(&fb)
        }
        ColumnId::FilePath => a.path.cmp(&b.path),
        ColumnId::NowPlaying => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("music_player_rs_test");
        let path = dir.join("playlist.m3u");
        let tracks = vec![
            track_for_path(Path::new("/music/a.flac")),
            track_for_path(Path::new("/music/b.wav")),
        ];
        assert!(save_track_list(&path, &tracks));
        let loaded = load_track_list(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].path, PathBuf::from("/music/a.flac"));
        assert_eq!(loaded[1].path, PathBuf::from("/music/b.wav"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_track_list_skips_non_audio_and_comments() {
        let dir = std::env::temp_dir().join("music_player_rs_test");
        let path = dir.join("comments.m3u");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "#EXTM3U\n/unsupported.txt\n/music/c.mp3\n").unwrap();
        let loaded = load_track_list(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, PathBuf::from("/music/c.mp3"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn advance_index_wraps_on_repeat_all() {
        use crate::settings::RepeatMode;
        assert_eq!(advance_index(Some(4), 1, 5, RepeatMode::All), Some(0));
        assert_eq!(advance_index(Some(0), -1, 5, RepeatMode::All), Some(4));
        assert_eq!(advance_index(Some(2), -1, 5, RepeatMode::All), Some(1));
    }

    #[test]
    fn advance_index_clamps_without_repeat() {
        use crate::settings::RepeatMode;
        assert_eq!(advance_index(Some(4), 1, 5, RepeatMode::Off), None);
        assert_eq!(advance_index(Some(0), -1, 5, RepeatMode::Off), None);
        assert_eq!(advance_index(Some(2), 1, 5, RepeatMode::One), Some(3));
        assert_eq!(advance_index(None, 1, 5, RepeatMode::Off), Some(1));
    }

    #[test]
    fn advance_shuffle_navigates_order() {
        use crate::settings::RepeatMode;
        let order = vec![3usize, 1, 4, 0, 2];
        assert_eq!(advance_shuffle(&order, 0, 1, RepeatMode::Off), Some((1, 1)));
        assert_eq!(advance_shuffle(&order, 4, 1, RepeatMode::Off), None);
        assert_eq!(advance_shuffle(&order, 4, 1, RepeatMode::All), Some((3, 0)));
        assert_eq!(
            advance_shuffle(&order, 0, -1, RepeatMode::All),
            Some((2, 4))
        );
    }
}
