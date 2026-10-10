use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use walkdir::WalkDir;

use crate::platform::fs::{FileWriter, WriteError};
use crate::settings::ColumnId;

pub mod compare;
pub mod load;
pub mod model;
pub mod shuffle;

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

/// Linear (non-shuffle) navigation destination. Indices are positions in the
/// visible order (`Playlist::visible`, §3.1). `direction` is +1 for next, -1
/// for prev. `current == None` always returns the first visible track
/// (§7.5, ТЗ-46), regardless of direction/repeat. Repeat::All wraps;
/// Off/One advance without wrapping (manual navigation under Repeat One
/// still moves to the next track).
pub fn advance_index(
    current: Option<usize>,
    direction: i32,
    n: usize,
    repeat: crate::settings::RepeatMode,
) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let Some(cur) = current else {
        return Some(0);
    };
    let cur = cur.min(n - 1) as i64;
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

/// Сериализует плейлист в байты plain M3U (один путь на строку) — снимок для
/// писателя (ADR-1, §6.5).
pub fn serialize_m3u(tracks: &[Track]) -> Arc<[u8]> {
    let mut out = String::new();
    for t in tracks {
        out.push_str(&t.path.to_string_lossy());
        out.push('\n');
    }
    Arc::from(out.into_bytes())
}

/// Save the playlist as a plain M3U file: атомарная долговечная запись через
/// модуль ФС (ТЗ-18, ТЗ-19); ошибка — вызывающему для журнала (ТЗ-20).
pub fn save_track_list(fs: &mut dyn FileWriter, path: &Path, tracks: &[Track]) -> Result<(), WriteError> {
    fs.write_atomic(path, &serialize_m3u(tracks))
}

/// Экспорт «Сохранить плейлист» в расширенный M3U: видимый порядок (без
/// перестановки Shuffle, выбирает вызывающий), `#EXTINF` с длительностью в
/// целых секундах (`-1`, если неизвестна) и подписью «Исполнитель — Название»
/// (ADR-17, §6.18, ТЗ-13 а).
pub fn serialize_extm3u<'a>(tracks: impl IntoIterator<Item = &'a Track>) -> Arc<[u8]> {
    let mut out = String::from("#EXTM3U\n");
    for t in tracks {
        let secs = t.duration.map(|d| d.round() as i64).unwrap_or(-1);
        let artist = t.artist.as_deref().unwrap_or("");
        let label = match (artist.is_empty(), t.title.is_empty()) {
            (false, false) => format!("{artist} — {}", t.title),
            (false, true) => artist.to_string(),
            (true, false) => t.title.clone(),
            (true, true) => t
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| t.path.to_string_lossy().into_owned()),
        };
        out.push_str(&format!("#EXTINF:{secs},{label}\n"));
        out.push_str(&t.path.to_string_lossy());
        out.push('\n');
    }
    Arc::from(out.into_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// ТЗ-18, ТЗ-20: запись через модуль ФС; ошибка не меняет файл на диске.
    #[test]
    fn save_track_list_via_file_writer() {
        use crate::platform::fs::{MemStore, WriteErrorClass, WriteStep};
        let path = Path::new("/cfg/playlist.m3u");
        let mut mem = MemStore::new();
        let tracks = vec![track_for_path(Path::new("/music/a.flac"))];
        save_track_list(&mut mem, path, &tracks).expect("save");
        assert_eq!(mem.get(path).as_deref(), Some(&b"/music/a.flac\n"[..]));
        mem.fail_write(path, WriteStep::SyncFile, WriteErrorClass::ReadOnlyFs, 1);
        let err = save_track_list(&mut mem, path, &[]).expect_err("injected");
        assert_eq!(err.class, WriteErrorClass::ReadOnlyFs);
        assert_eq!(mem.get(path).as_deref(), Some(&b"/music/a.flac\n"[..]));
    }

    /// ADR-1, §6.5: снимок плейлиста — по пути на строку, пустой список — пустые байты.
    #[test]
    fn serialize_m3u_one_path_per_line() {
        let tracks = vec![
            track_for_path(Path::new("/music/a.flac")),
            track_for_path(Path::new("/music/b.wav")),
        ];
        assert_eq!(&*serialize_m3u(&tracks), &b"/music/a.flac\n/music/b.wav\n"[..]);
        assert!(serialize_m3u(&[]).is_empty());
    }

    #[test]
    fn playlist_save_writes_plain_m3u() {
        let dir = std::env::temp_dir().join("music_player_rs_test");
        let path = dir.join("playlist.m3u");
        let tracks = vec![
            track_for_path(Path::new("/music/a.flac")),
            track_for_path(Path::new("/music/b.wav")),
        ];
        let journal = std::sync::Arc::new(crate::journal::VecJournal::default());
        let (_, mut fs, _) = crate::platform::fs::os_fs(journal);
        save_track_list(fs.as_mut(), &path, &tracks).expect("save");
        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(written, "/music/a.flac\n/music/b.wav\n");
        let _ = std::fs::remove_file(&path);
    }

    /// §7, ТЗ-40: Repeat All wraps at both ends of the visible order; Off/One
    /// clamp instead of wrapping.
    #[test]
    fn repeat_all_wraps_visible_order() {
        use crate::settings::RepeatMode;
        assert_eq!(advance_index(Some(4), 1, 5, RepeatMode::All), Some(0));
        assert_eq!(advance_index(Some(0), -1, 5, RepeatMode::All), Some(4));
        assert_eq!(advance_index(Some(2), -1, 5, RepeatMode::All), Some(1));
        assert_eq!(advance_index(Some(4), 1, 5, RepeatMode::Off), None);
        assert_eq!(advance_index(Some(0), -1, 5, RepeatMode::Off), None);
        assert_eq!(advance_index(Some(2), 1, 5, RepeatMode::One), Some(3));
    }

    /// §7, ТЗ-46: «Далее»/«Назад» without a current track plays the first
    /// track of the visible order, regardless of direction/repeat.
    #[test]
    fn next_without_current_plays_first_visible() {
        use crate::settings::RepeatMode;
        assert_eq!(advance_index(None, 1, 5, RepeatMode::Off), Some(0));
        assert_eq!(advance_index(None, 1, 5, RepeatMode::All), Some(0));
        assert_eq!(advance_index(None, 1, 0, RepeatMode::Off), None);
    }

    /// ADR-17, §6.18, ТЗ-13 а: первая строка `#EXTM3U`, без BOM, окончания `\n`.
    #[test]
    fn serialize_extm3u_header_no_bom_lf_endings() {
        let out = serialize_extm3u(&[]);
        assert_eq!(&*out, b"#EXTM3U\n");
        assert!(!out.starts_with(&[0xEF, 0xBB, 0xBF]));
        assert!(!out.contains(&b'\r'));
    }

    /// ADR-17, §6.18, ТЗ-13 а: оба поля непустые → «Исполнитель — Название» (тире U+2014 с пробелами).
    #[test]
    fn serialize_extm3u_label_artist_and_title() {
        let t = Track {
            path: PathBuf::from("/music/a.flac"),
            title: "Название".into(),
            artist: Some("Исполнитель".into()),
            duration: Some(125.4),
            ..Default::default()
        };
        let text = String::from_utf8(serialize_extm3u(&[t]).to_vec()).expect("utf8");
        assert_eq!(text, "#EXTM3U\n#EXTINF:125,Исполнитель — Название\n/music/a.flac\n");
    }

    /// ADR-17, §6.18, ТЗ-13 а: только одно из полей непустое → подпись — это поле.
    #[test]
    fn serialize_extm3u_label_single_field() {
        let artist_only = Track {
            path: PathBuf::from("/music/b.flac"),
            title: String::new(),
            artist: Some("Исполнитель".into()),
            duration: None,
            ..Default::default()
        };
        let title_only = Track {
            path: PathBuf::from("/music/c.flac"),
            title: "Название".into(),
            artist: None,
            duration: None,
            ..Default::default()
        };
        let text = String::from_utf8(serialize_extm3u(&[artist_only, title_only]).to_vec()).expect("utf8");
        assert_eq!(
            text,
            "#EXTM3U\n#EXTINF:-1,Исполнитель\n/music/b.flac\n#EXTINF:-1,Название\n/music/c.flac\n"
        );
    }

    /// ADR-17, §6.18, ТЗ-13 а: оба поля пусты → подпись — имя файла без расширения.
    #[test]
    fn serialize_extm3u_label_falls_back_to_file_stem() {
        let t = Track {
            path: PathBuf::from("/music/no_tags.flac"),
            title: String::new(),
            artist: None,
            duration: None,
            ..Default::default()
        };
        let text = String::from_utf8(serialize_extm3u(&[t]).to_vec()).expect("utf8");
        assert_eq!(text, "#EXTM3U\n#EXTINF:-1,no_tags\n/music/no_tags.flac\n");
    }

    /// ADR-17, §6.18, ТЗ-13 а: неизвестная длительность → `-1`; известная — округлена до целого.
    #[test]
    fn serialize_extm3u_unknown_duration_is_minus_one() {
        let known = Track {
            path: PathBuf::from("/music/d.flac"),
            title: "T".into(),
            duration: Some(59.6),
            ..Default::default()
        };
        let unknown = Track {
            path: PathBuf::from("/music/e.flac"),
            title: "T".into(),
            duration: None,
            ..Default::default()
        };
        let text = String::from_utf8(serialize_extm3u(&[known, unknown]).to_vec()).expect("utf8");
        assert!(text.contains("#EXTINF:60,T\n/music/d.flac\n"));
        assert!(text.contains("#EXTINF:-1,T\n/music/e.flac\n"));
    }

    /// ADR-17, §6.18, ТЗ-13 а: порядок треков в выводе — порядок переданного итератора (видимый порядок caller'а).
    #[test]
    fn serialize_extm3u_preserves_caller_order() {
        let tracks = [
            Track {
                path: PathBuf::from("/music/z.flac"),
                title: "Z".into(),
                ..Default::default()
            },
            Track {
                path: PathBuf::from("/music/a.flac"),
                title: "A".into(),
                ..Default::default()
            },
        ];
        let text = String::from_utf8(serialize_extm3u(&tracks).to_vec()).expect("utf8");
        let z_pos = text.find("/music/z.flac").expect("z present");
        let a_pos = text.find("/music/a.flac").expect("a present");
        assert!(z_pos < a_pos);
    }
}
