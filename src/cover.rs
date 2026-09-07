//! Разрешение обложек альбомов: из папки → встроенная (кэшируется на диск) →
//! из интернета (iTunes Search API). Работает в фоновом потоке.
//!
//! Результат — путь к файлу изображения, который Slint показывает через
//! `Image::load_from_path` (декодирование PNG/JPEG встроено в `slint/std`).

use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardVisualKey};

use crate::playlist::Track;
use crate::settings::Settings;

/// Запрос на подгрузку обложки для трека.
#[derive(Clone)]
pub struct CoverJob {
    /// Идентификатор запроса; на UI применяется только результат с
    /// `id == текущему счётчику`, что защищает от устаревших результатов.
    pub id: u64,
    pub track: Track,
    pub cfg: CoverConfig,
}

/// Результат фонового резолва: путь к картинке или `None`.
pub struct CoverDone {
    pub id: u64,
    pub image: Option<PathBuf>,
}

/// Лёгкий снэпшот настроек обложек для передачи в фоновый поток.
#[derive(Clone)]
pub struct CoverConfig {
    /// Ключи источников (`CoverSource::key`) в порядке приоритета.
    pub priority: Vec<String>,
    /// Имена файлов обложек, искомых в папке с альбомом.
    pub folder_names: Vec<String>,
    /// Искать обложки в интернете.
    pub online: bool,
}

impl CoverConfig {
    pub fn from_settings(s: &Settings) -> Self {
        CoverConfig {
            priority: s
                .cover_priority_ordered()
                .iter()
                .map(|c| c.key().to_string())
                .collect(),
            folder_names: s.cover_folder_names_list(),
            online: s.cover_online,
        }
    }
}

/// Директория кэша обложек: `$XDG_CACHE_HOME/music_player/covers`
/// (по умолчанию `~/.cache/music_player/covers`).
pub fn covers_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("music_player")
        .join("covers")
}

/// Фоновый поток: принимает `CoverJob`, резолвит обложку и отправляет
/// `CoverDone`. Перед обработкой сбрасывает уже поставленные в очередь
/// джобы, оставляя самый свежий (быстрое переключение треков).
pub fn start_worker(rx: mpsc::Receiver<CoverJob>, done: mpsc::Sender<CoverDone>) {
    std::thread::spawn(move || loop {
        // Отбрасываем всё, что поставили в очередь, пока мы заняты.
        while rx.try_recv().is_ok() {}
        let job = match rx.recv() {
            Ok(j) => j,
            Err(_) => return,
        };
        let image = resolve_cover(&job.track, &job.cfg);
        let _ = done.send(CoverDone { id: job.id, image });
    });
}

/// Пройти по источникам в порядке приоритета, вернуть первый найденный путь.
pub fn resolve_cover(track: &Track, cfg: &CoverConfig) -> Option<PathBuf> {
    let cache = covers_cache_dir();
    for key in &cfg.priority {
        match key.as_str() {
            "folder" => {
                if let Some(p) = folder_cover(track.path.parent(), &cfg.folder_names) {
                    return Some(p);
                }
            }
            "embedded" => {
                if let Some(p) = embedded_cover(&track.path, &cache) {
                    return Some(p);
                }
            }
            "internet" if cfg.online => {
                if let Some(p) = internet_cover(track, &cache) {
                    return Some(p);
                }
            }
            _ => {}
        }
    }
    None
}

/// Обложка из папки с альбомом: первый существующий файл из списка имён.
pub fn folder_cover(dir: Option<&Path>, names: &[String]) -> Option<PathBuf> {
    let dir = dir?;
    for n in names {
        let p = dir.join(n);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Встроенная обложка (APIC/covr/pictures). Извлекается один раз и пишется в
/// кэш-директорию; при следующих обращениях читается из кэша.
pub fn embedded_cover(path: &Path, cache: &Path) -> Option<PathBuf> {
    let hash = hash_str(&format!("embedded|{}", path.display()));
    if let Some(p) = find_cached(cache, hash) {
        return Some(p);
    }
    let (data, media_type) = probe_visual(path)?;
    let ext = sniff_ext(&data)
        .or_else(|| {
            media_type
                .filter(|m| m == "image/jpeg" || m == "image/png")
                .map(|m| ext_from_mime(&m))
        })
        .filter(|e| matches!(*e, "jpg" | "png"))
        .unwrap_or("jpg");
    write_cover(cache, hash, &data, ext)
}

/// Поиск по iTunes Search API: первый результат альбома, арт в 600×600.
pub fn internet_cover(track: &Track, cache: &Path) -> Option<PathBuf> {
    let artist = track.artist.as_deref().unwrap_or("").trim();
    let album = track.album.as_deref().unwrap_or("").trim();
    if artist.is_empty() && album.is_empty() {
        return None;
    }
    let term = format!("{artist} {album}");
    let hash = hash_str(&format!("internet|{term}"));
    if let Some(p) = find_cached(cache, hash) {
        return Some(p);
    }

    let url = format!(
        "https://itunes.apple.com/search?term={}&entity=album&limit=5",
        percent_encode(&term)
    );
    let body = ureq::get(&url).call().ok()?.into_string().ok()?;
    let artwork: String = serde_json::from_str::<serde_json::Value>(&body)
        .ok()?
        .get("results")?
        .as_array()?
        .iter()
        .filter_map(|r| r.get("artworkUrl100").and_then(|v| v.as_str()))
        .next()?
        .to_string();
    // iTunes отдаёт превью 100×100; просим арт побольше.
    let art = artwork.replace("100x100", "600x600");

    let mut bytes = Vec::new();
    ureq::get(&art).call().ok()?.into_reader().read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let ext = sniff_ext(&bytes).unwrap_or("jpg");
    write_cover(cache, hash, &bytes, ext)
}

/// Извлечь первый подходящий `Visual` (front cover или любой) из symphonia.
fn probe_visual(path: &Path) -> Option<(Box<[u8]>, Option<String>)> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .ok()?;
    let meta = format.metadata();
    let rev = meta.current()?;
    let visuals = &rev.media.visuals;
    let v = visuals
        .iter()
        .find(|v| v.usage == Some(StandardVisualKey::FrontCover))
        .or_else(|| visuals.first())?;
    Some((v.data.clone(), v.media_type.clone()))
}

/// Расширение по magic-байтам картинки.
pub fn sniff_ext(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xFF, 0xD8]) {
        Some("jpg")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if data.starts_with(b"GIF8") {
        Some("gif")
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        Some("webp")
    } else {
        None
    }
}

/// Расширение по MIME-типу (только jpeg/png/svg — то, что умеет Slint из коробки).
fn ext_from_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/svg+xml" => "svg",
        _ => "jpg",
    }
}

/// Детерминированный хэш строки для имён кэш-файлов (стабилен между запусками).
pub fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn cache_prefix(hash: u64) -> String {
    format!("{hash:016x}")
}

fn find_cached(dir: &Path, hash: u64) -> Option<PathBuf> {
    let prefix = cache_prefix(hash);
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) {
            return Some(e.path());
        }
    }
    None
}

fn write_cover(dir: &Path, hash: u64, data: &[u8], ext: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let p = dir.join(format!("{}.{ext}", cache_prefix(hash)));
    std::fs::write(&p, data).ok()?;
    Some(p)
}

/// Простейший percent-encoding по RFC 3986 (для строки поиска iTunes).
pub fn percent_encode(s: &str) -> String {
    const SAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        if SAFE.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::CoverSource;

    #[test]
    fn percent_encode_encodes_space_and_unicode() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("Tame Impala Currents"), "Tame%20Impala%20Currents");
        assert_eq!(percent_encode("Пётр"), "%D0%9F%D1%91%D1%82%D1%80");
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash_str("a/b/c.flac"), hash_str("a/b/c.flac"));
        assert_ne!(hash_str("a/b/c.flac"), hash_str("a/b/d.flac"));
    }

    #[test]
    fn sniff_detects_jpeg_and_png() {
        assert_eq!(sniff_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(sniff_ext(b"\x89PNG\r\n\x1a\n..."), Some("png"));
        assert_eq!(sniff_ext(b"GIF89a"), Some("gif"));
        assert_eq!(sniff_ext(b"not an image"), None);
    }

    #[test]
    fn folder_cover_respects_name_order() {
        let dir = std::env::temp_dir().join(format!("music_player_cover_test_{}", hash_str("folder")));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("art.png"), b"png");
        let names: Vec<String> = ["cover.jpg", "art.png", "album.jpg"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(folder_cover(Some(&dir), &names), Some(dir.join("art.png")));
        // Несуществующая папка → None.
        assert_eq!(
            folder_cover(Some(&dir.join("missing")), &names),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_cover_returns_existing_cache() {
        let dir = std::env::temp_dir().join(format!("music_player_cover_write_{}", hash_str("write")));
        let hash = hash_str("fixture");
        write_cover(&dir, hash, b"xyz", "png");
        assert_eq!(find_cached(&dir, hash), Some(dir.join(format!("{}.png", cache_prefix(hash)))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cover_priority_keys_roundtrip() {
        for c in CoverSource::ALL {
            assert_eq!(CoverSource::from_key(c.key()), Some(c));
        }
    }
}