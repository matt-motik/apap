//! Ключи сравнения строк плейлиста (§3.1, §6.14, ТЗ-43, ОВС-3 а).

use std::cmp::Ordering;

use super::model::{SortColumn, SortDir, SortKey};
use super::Track;

/// Текст после trim, to_lowercase и NFC (ОВС-3 а), разбитый на текстовые и числовые отрезки (§3.1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextKey(Box<[Seg]>);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Seg {
    Text(Box<str>),
    /// ASCII-цифры без ведущих нулей; пустая строка — значение 0.
    Num(Box<str>),
}

impl TextKey {
    /// Строит ключ сравнения из исходной строки (§6.14): trim, to_lowercase, NFC,
    /// затем разбиение на максимальные пробеги ASCII-цифр (`Num`, без ведущих нулей)
    /// и остальных символов (`Text`).
    pub fn new(raw: &str) -> TextKey {
        let trimmed = raw.trim().to_lowercase();
        let normalized = icu_normalizer::ComposingNormalizerBorrowed::new_nfc().normalize(&trimmed);

        let mut segments = Vec::new();
        let mut chars = normalized.chars().peekable();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                let mut digits = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        digits.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let stripped = digits.trim_start_matches('0');
                segments.push(Seg::Num(stripped.into()));
            } else {
                let mut text = String::new();
                while let Some(&t) = chars.peek() {
                    if t.is_ascii_digit() {
                        break;
                    }
                    text.push(t);
                    chars.next();
                }
                segments.push(Seg::Text(text.into()));
            }
        }

        TextKey(segments.into())
    }

    /// `true`, если ключ не содержит отрезков (исходная строка была пустой после trim).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Натуральное сравнение ключей (§6.14): по отрезкам, `Num` сравниваются по длине и
/// затем лексикографически (= по числовому значению без переполнения), `Text` — по
/// кодовым точкам, `Num` всегда меньше `Text`; при равенстве общих отрезков короче ключ меньше.
pub fn cmp_text(a: &TextKey, b: &TextKey) -> Ordering {
    for (sa, sb) in a.0.iter().zip(b.0.iter()) {
        let ord = match (sa, sb) {
            (Seg::Num(na), Seg::Num(nb)) => na.len().cmp(&nb.len()).then_with(|| na.cmp(nb)),
            (Seg::Text(ta), Seg::Text(tb)) => ta.cmp(tb),
            (Seg::Num(_), Seg::Text(_)) => Ordering::Less,
            (Seg::Text(_), Seg::Num(_)) => Ordering::Greater,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.0.len().cmp(&b.0.len())
}

/// Ключи сравнения строки, вычисленные при загрузке и обновлении тегов (ADR-15, §3.1).
#[derive(Clone, PartialEq, Debug)]
pub struct CompareKeys {
    pub title: TextKey,
    pub artist: TextKey,
    pub album: TextKey,
    pub genre: TextKey,
    pub format: TextKey,
    pub bit_depth: TextKey,
    pub file_name: TextKey,
    pub file_path: TextKey,
    /// `None` — пусто: в конце при любом направлении (ТЗ-43).
    pub year: Option<u32>,
    pub track_no: Option<u32>,
    pub disc: Option<u32>,
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub duration_ms: Option<u64>,
}

impl CompareKeys {
    /// Ключи из полей `Track` (§6.14): год — первая серия ASCII-цифр; нулевые номер трека,
    /// диск, битрейт и частота — «неизвестно» (`None`); имя и путь файла — из `path`.
    pub fn from_track(t: &Track) -> CompareKeys {
        CompareKeys {
            title: TextKey::new(&t.title),
            artist: TextKey::new(t.artist.as_deref().unwrap_or("")),
            album: TextKey::new(t.album.as_deref().unwrap_or("")),
            genre: TextKey::new(t.genre.as_deref().unwrap_or("")),
            format: TextKey::new(&t.format),
            bit_depth: TextKey::new(&t.bit_depth),
            file_name: TextKey::new(&t.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
            file_path: TextKey::new(&t.path.to_string_lossy()),
            year: first_digit_run(&t.year),
            track_no: known(t.track_number),
            disc: known(t.disc),
            bitrate: known(t.bitrate),
            sample_rate: known(t.sample_rate),
            duration_ms: t.duration.and_then(duration_ms),
        }
    }
}

/// 0 в полях `Track` означает «неизвестно» (§6.14).
fn known(v: u32) -> Option<u32> {
    (v != 0).then_some(v)
}

/// Первая серия ASCII-цифр строки года; нет цифр (или не помещается в `u32`) → `None` (§6.14).
fn first_digit_run(s: &str) -> Option<u32> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let rest = &s[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Длительность в секундах → миллисекунды; неизвестная, отрицательная или
/// непредставимая → `None` (§6.14).
fn duration_ms(secs: f64) -> Option<u64> {
    let d = std::time::Duration::try_from_secs_f64(secs).ok()?;
    u64::try_from(d.as_millis()).ok()
}

/// Сравнение двух значений одного поля: пустое (`None`) всегда после непустого,
/// направление `dir` разворачивает только сравнение непустых (§6.14, ТЗ-43).
fn cmp_field<T>(a: Option<T>, b: Option<T>, dir: SortDir, cmp: impl FnOnce(T, T) -> Ordering) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => match dir {
            SortDir::Asc => cmp(x, y),
            SortDir::Desc => cmp(x, y).reverse(),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn non_empty(k: &TextKey) -> Option<&TextKey> {
    (!k.is_empty()).then_some(k)
}

fn cmp_tk(a: &TextKey, b: &TextKey, dir: SortDir) -> Ordering {
    cmp_field(non_empty(a), non_empty(b), dir, cmp_text)
}

fn cmp_num<T: Ord>(a: Option<T>, b: Option<T>, dir: SortDir) -> Ordering {
    cmp_field(a, b, dir, |x, y| x.cmp(&y))
}

/// Сравнение строк по ключу (§6.14, ТЗ-43): первичное поле колонки ключа, затем
/// вторичные по возрастанию (Исполнитель → год, альбом, диск, трек; Альбом → диск,
/// трек; Год → альбом, диск, трек). Полное равенство — `Equal`: порядок равных
/// решает исходный индекс (§6.13).
pub fn compare_keys(a: &CompareKeys, b: &CompareKeys, key: SortKey) -> Ordering {
    let d = key.dir;
    let asc = SortDir::Asc;
    match key.column {
        SortColumn::TrackNumber => cmp_num(a.track_no, b.track_no, d),
        SortColumn::Title => cmp_tk(&a.title, &b.title, d),
        SortColumn::Artist => cmp_tk(&a.artist, &b.artist, d)
            .then_with(|| cmp_num(a.year, b.year, asc))
            .then_with(|| cmp_tk(&a.album, &b.album, asc))
            .then_with(|| cmp_num(a.disc, b.disc, asc))
            .then_with(|| cmp_num(a.track_no, b.track_no, asc)),
        SortColumn::Album => cmp_tk(&a.album, &b.album, d)
            .then_with(|| cmp_num(a.disc, b.disc, asc))
            .then_with(|| cmp_num(a.track_no, b.track_no, asc)),
        SortColumn::Genre => cmp_tk(&a.genre, &b.genre, d),
        SortColumn::Year => cmp_num(a.year, b.year, d)
            .then_with(|| cmp_tk(&a.album, &b.album, asc))
            .then_with(|| cmp_num(a.disc, b.disc, asc))
            .then_with(|| cmp_num(a.track_no, b.track_no, asc)),
        SortColumn::Format => cmp_tk(&a.format, &b.format, d),
        SortColumn::Bitrate => cmp_num(a.bitrate, b.bitrate, d),
        SortColumn::BitDepth => cmp_tk(&a.bit_depth, &b.bit_depth, d),
        SortColumn::SampleRate => cmp_num(a.sample_rate, b.sample_rate, d),
        SortColumn::Duration => cmp_num(a.duration_ms, b.duration_ms, d),
        SortColumn::FileName => cmp_tk(&a.file_name, &b.file_name, d),
        SortColumn::FilePath => cmp_tk(&a.file_path, &b.file_path, d),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_key_splits_numbers_and_strips_zeros() {
        let key = TextKey::new("Track 007 - Intro");
        assert_eq!(
            key.0.as_ref(),
            &[
                Seg::Text("track ".into()),
                Seg::Num("7".into()),
                Seg::Text(" - intro".into()),
            ]
        );
    }

    #[test]
    fn text_key_nfc_equal() {
        let precomposed = TextKey::new("caf\u{00e9}");
        let decomposed = TextKey::new("cafe\u{0301}");
        assert_eq!(precomposed, decomposed);
    }

    #[test]
    fn cmp_text_natural_order() {
        let a = TextKey::new("no. 9");
        let b = TextKey::new("no. 10");
        assert_eq!(cmp_text(&a, &b), Ordering::Less);
    }

    #[test]
    fn text_key_empty_after_trim() {
        let key = TextKey::new("   ");
        assert!(key.is_empty());
    }

    fn keys(f: impl FnOnce(&mut Track)) -> CompareKeys {
        let mut t = Track::default();
        f(&mut t);
        CompareKeys::from_track(&t)
    }

    /// Порядок индексов по ключу; равные — по исходному индексу (§6.13).
    fn order(rows: &[CompareKeys], key: SortKey) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..rows.len()).collect();
        idx.sort_by(|&a, &b| compare_keys(&rows[a], &rows[b], key).then(a.cmp(&b)));
        idx
    }

    fn asc(column: SortColumn) -> SortKey {
        SortKey { column, dir: SortDir::Asc }
    }

    fn desc(column: SortColumn) -> SortKey {
        SortKey { column, dir: SortDir::Desc }
    }

    /// Правила сравнения всех колонок в обоих направлениях: регистр, числа внутри
    /// строк, пустые в конце, вторичные ключи по возрастанию, NFC = NFD (ТЗ-43, §6.14).
    #[test]
    fn compare_rules_all_columns_both_directions() {
        use std::path::PathBuf;
        // Для каждой колонки: [больший, меньший, пустой].
        let cases: Vec<(SortColumn, [CompareKeys; 3])> = vec![
            (
                SortColumn::TrackNumber,
                [keys(|t| t.track_number = 10), keys(|t| t.track_number = 9), keys(|_| {})],
            ),
            (
                SortColumn::Title,
                [keys(|t| t.title = "No. 10".into()), keys(|t| t.title = "No. 9".into()), keys(|_| {})],
            ),
            (
                SortColumn::Artist,
                [
                    keys(|t| t.artist = Some("Zappa".into())),
                    keys(|t| t.artist = Some("beatles".into())),
                    keys(|t| t.artist = Some("  ".into())),
                ],
            ),
            (
                SortColumn::Album,
                [
                    keys(|t| t.album = Some("Hot Rats".into())),
                    keys(|t| t.album = Some("abbey road".into())),
                    keys(|_| {}),
                ],
            ),
            (
                SortColumn::Genre,
                [keys(|t| t.genre = Some("Rock".into())), keys(|t| t.genre = Some("jazz".into())), keys(|_| {})],
            ),
            (
                SortColumn::Year,
                [keys(|t| t.year = "2001".into()), keys(|t| t.year = "999".into()), keys(|t| t.year = String::new())],
            ),
            (
                SortColumn::Format,
                [keys(|t| t.format = "WAV".into()), keys(|t| t.format = "flac".into()), keys(|_| {})],
            ),
            (
                SortColumn::Bitrate,
                [keys(|t| t.bitrate = 1411), keys(|t| t.bitrate = 320), keys(|_| {})],
            ),
            (
                SortColumn::BitDepth,
                [keys(|t| t.bit_depth = "24".into()), keys(|t| t.bit_depth = "16".into()), keys(|_| {})],
            ),
            (
                SortColumn::SampleRate,
                [keys(|t| t.sample_rate = 96000), keys(|t| t.sample_rate = 44100), keys(|_| {})],
            ),
            (
                SortColumn::Duration,
                [keys(|t| t.duration = Some(300.0)), keys(|t| t.duration = Some(9.5)), keys(|_| {})],
            ),
            (
                SortColumn::FileName,
                [
                    keys(|t| t.path = PathBuf::from("/b/track 10.flac")),
                    keys(|t| t.path = PathBuf::from("/z/track 9.flac")),
                    keys(|_| {}),
                ],
            ),
            (
                SortColumn::FilePath,
                [
                    keys(|t| t.path = PathBuf::from("/z/a.flac")),
                    keys(|t| t.path = PathBuf::from("/b/z.flac")),
                    keys(|_| {}),
                ],
            ),
        ];
        for (column, rows) in &cases {
            assert_eq!(order(rows, asc(*column)), vec![1, 0, 2], "{column:?} asc");
            assert_eq!(order(rows, desc(*column)), vec![0, 1, 2], "{column:?} desc");
        }

        // Год без цифр — пустой, в конце в обоих направлениях.
        let years = [keys(|t| t.year = "n/a".into()), keys(|t| t.year = "1969-05".into())];
        assert_eq!(years[1].year, Some(1969));
        assert_eq!(order(&years, asc(SortColumn::Year)), vec![1, 0]);
        assert_eq!(order(&years, desc(SortColumn::Year)), vec![1, 0]);

        // Диски 1/2 одного альбома: вторичные ключи по возрастанию при любом направлении.
        let album = |disc: u32, no: u32| {
            keys(move |t| {
                t.artist = Some("beatles".into());
                t.album = Some("Abbey Road".into());
                t.year = "1969".into();
                t.disc = disc;
                t.track_number = no;
            })
        };
        let discs = [album(2, 1), album(1, 2), album(1, 1)];
        for column in [SortColumn::Artist, SortColumn::Album, SortColumn::Year] {
            assert_eq!(order(&discs, asc(column)), vec![2, 1, 0], "{column:?} asc");
            assert_eq!(order(&discs, desc(column)), vec![2, 1, 0], "{column:?} desc");
        }

        // Исполнитель → год → альбом: вторичный год по возрастанию и при «↓».
        let by_year = [
            keys(|t| {
                t.artist = Some("Zappa".into());
                t.year = "1974".into();
            }),
            keys(|t| {
                t.artist = Some("Zappa".into());
                t.year = "1969".into();
            }),
            keys(|t| t.artist = Some("beatles".into())),
        ];
        assert_eq!(order(&by_year, asc(SortColumn::Artist)), vec![2, 1, 0]);
        assert_eq!(order(&by_year, desc(SortColumn::Artist)), vec![1, 0, 2]);

        // Одно название в NFC и NFD — равные ключи, порядок решает исходный индекс.
        let nfc = keys(|t| t.title = "Caf\u{e9}".into());
        let nfd = keys(|t| t.title = "Cafe\u{301}".into());
        assert_eq!(compare_keys(&nfc, &nfd, asc(SortColumn::Title)), Ordering::Equal);
        assert_eq!(compare_keys(&nfc, &nfd, desc(SortColumn::Title)), Ordering::Equal);
    }
}
