//! Разбор `settings.toml` / `state.toml` по ключам (ТЗ-5, §2.3, §6.2).
//!
//! Таблица `KeySpec` — единственный источник перечня ключей файла, их типов,
//! допустимых множеств и значений по умолчанию (ТЗ-2, ТЗ-8). Сам обход
//! (`walk`) — чистая функция без ввода-вывода (ТЗ-4); пост-проверки с
//! заметкой `Adjusted` (`column_order`, `cover_priority`, `sort.*`) и
//! исключение раздела `[playback]` из «неизвестных» ключей — дело вызывающей
//! стороны (§6.2).

use crate::platform::fs::ReadError;
use std::sync::Arc;

/// Путь ключа: `"theme"`, `"columns.title.visible"`, `"playback.optimal.volume"` (§2.3).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct KeyPath(Box<str>);

impl KeyPath {
    /// Путь ключа из точечной строки.
    pub fn new(path: impl Into<Box<str>>) -> KeyPath {
        KeyPath(path.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for KeyPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Заметка разбора (ТЗ-5). Тот же тип возвращает `load_mode_settings`
/// (01_audio_modes), где он не определён (§11).
#[derive(Clone, PartialEq, Debug)]
pub struct LoadNote {
    pub key: KeyPath,
    pub kind: LoadNoteKind,
}

#[derive(Clone, PartialEq, Debug)]
pub enum LoadNoteKind {
    /// Обязательный ключ отсутствует → значение по умолчанию (ТЗ-5, ТЗ-8).
    Missing,
    /// Неверный тип или значение вне допустимого множества → значение по умолчанию.
    Invalid { found: Box<str>, allowed: &'static str },
    /// Неизвестный ключ: в память не попадает и при следующей записи файла исчезает (ТЗ-5, ТЗ-9).
    Unknown,
    /// Допустимое значение изменено правилом согласования.
    Adjusted { reason: Box<str> },
}

/// Извлечь значение листа в `T`; `Err` — текст допустимого множества для заметки `Invalid`.
type ReadFn<T> = dyn Fn(&toml::Value, &mut T) -> Result<(), &'static str>;

/// Строка таблицы описаний ключей (§2.3, §6.2).
pub struct KeySpec<T: 'static> {
    pub path: String,
    /// `true` — отсутствие ключа означает «не задано» (`None`) и заметки не даёт
    /// (положение и размер окна, ширины колонок, ключ сортировки, последний каталог).
    pub optional: bool,
    /// Извлечь значение в `T`; `Err` — текст допустимого множества для заметки `Invalid`.
    /// Замыкание (не `fn`), т.к. листовые ключи колонок/подписей собираются
    /// циклом по `ColumnId`/`InfoLabelKey` и захватывают конкретный ключ (§6.2).
    pub read: Box<ReadFn<T>>,
    /// Записать в `T` значение по умолчанию.
    pub default: Box<dyn Fn(&mut T)>,
}

/// Результат чтения файла модулем ФС, вход чистой функции разбора (§2.3).
pub enum FileRead {
    Absent,
    Bytes(Arc<[u8]>),
    Failed(ReadError),
}

/// Результат разбора `settings.toml` / `state.toml`. Чистая функция, без
/// ввода-вывода (ТЗ-4, §2.3).
pub enum Parsed<T> {
    /// Файла нет: значения по умолчанию, эталон пуст, заметок нет (ТЗ-4, ТЗ-124 (01_audio_modes)).
    Absent { value: T },
    /// Документ TOML разобран. Значения — из файла с подстановкой по умолчанию; эталон — исходный текст.
    Parsed { value: T, notes: Vec<LoadNote>, reference: super::ReferenceText },
    /// Неразбираемый файл (ТЗ-6): значения по умолчанию, эталон пуст; байты — для копии `*.bad`.
    Unparsable { value: T, original: Arc<[u8]>, error: Box<str> },
    /// Ошибка чтения, отличная от «нет файла»: значения по умолчанию, запрет автоматических записей (ОВС-6 в).
    ReadFailed { value: T, err: ReadError },
}

/// Обход по ключам (ТЗ-5, §6.2). `value` собирается через `T::default()`, затем
/// по каждой строке `spec`: отсутствующий необязательный ключ не даёт заметки
/// (значение по умолчанию = «не задано»); отсутствующий обязательный —
/// `s.default` + `Missing`; ошибка `s.read` — `s.default` + `Invalid`.
/// Листовые пути документа, не встретившиеся ни в одной строке `spec`, дают
/// `Unknown`. Заметки сортируются по `KeyPath`.
pub fn walk<T: Default>(table: &toml::Table, spec: &[KeySpec<T>]) -> (T, Vec<LoadNote>) {
    let mut value = T::default();
    let mut notes = Vec::new();

    for s in spec {
        match lookup(table, &s.path) {
            None => {
                if !s.optional {
                    (s.default)(&mut value);
                    notes.push(LoadNote { key: KeyPath::new(s.path.as_str()), kind: LoadNoteKind::Missing });
                }
            }
            Some(v) => {
                if let Err(allowed) = (s.read)(v, &mut value) {
                    (s.default)(&mut value);
                    notes.push(LoadNote {
                        key: KeyPath::new(s.path.as_str()),
                        kind: LoadNoteKind::Invalid { found: v.to_string().into(), allowed },
                    });
                }
            }
        }
    }

    for path in leaf_paths(table) {
        if !spec.iter().any(|s| s.path == path.as_str()) {
            notes.push(LoadNote { key: path, kind: LoadNoteKind::Unknown });
        }
    }

    notes.sort_by(|a, b| a.key.cmp(&b.key));
    (value, notes)
}

/// Значение по точечному пути; промежуточная не-таблица или отсутствующий
/// ключ на любом уровне — отсутствие (§6.2).
fn lookup<'a>(table: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
    let mut parts = path.split('.');
    let first = parts.next()?;
    let mut current = table.get(first)?;
    for part in parts {
        current = current.as_table()?.get(part)?;
    }
    Some(current)
}

/// Все листовые (не-таблица) пути документа, точечной записью.
fn leaf_paths(table: &toml::Table) -> Vec<KeyPath> {
    let mut out = Vec::new();
    collect_leaf_paths(table, "", &mut out);
    out
}

fn collect_leaf_paths(table: &toml::Table, prefix: &str, out: &mut Vec<KeyPath>) {
    for (key, value) in table {
        let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
        match value {
            toml::Value::Table(nested) => collect_leaf_paths(nested, &path, out),
            _ => out.push(KeyPath::new(path)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, PartialEq, Debug)]
    struct Toy {
        a: i64,
        b: bool,
    }

    fn spec() -> Vec<KeySpec<Toy>> {
        vec![
            KeySpec {
                path: "a".to_string(),
                optional: false,
                read: Box::new(|v, t| {
                    t.a = v.as_integer().ok_or("целое число")?;
                    Ok(())
                }),
                default: Box::new(|t| t.a = -1),
            },
            KeySpec {
                path: "b.c".to_string(),
                optional: true,
                read: Box::new(|v, t| {
                    t.b = v.as_bool().ok_or("bool")?;
                    Ok(())
                }),
                default: Box::new(|t| t.b = false),
            },
        ]
    }

    fn table(text: &str) -> toml::Table {
        text.parse().expect("toml")
    }

    #[test]
    fn missing_required_key_gives_default_and_note() {
        let t = table("");
        let (value, notes) = walk(&t, &spec());
        assert_eq!(value, Toy { a: -1, b: false });
        assert_eq!(notes, vec![LoadNote { key: KeyPath::new("a"), kind: LoadNoteKind::Missing }]);
    }

    #[test]
    fn missing_optional_key_gives_no_note() {
        let t = table("a = 5");
        let (value, notes) = walk(&t, &spec());
        assert_eq!(value, Toy { a: 5, b: false });
        assert!(notes.is_empty());
    }

    #[test]
    fn invalid_value_gives_default_and_invalid_note() {
        let t = table("a = \"nope\"");
        let (value, notes) = walk(&t, &spec());
        assert_eq!(value.a, -1);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key, KeyPath::new("a"));
        match &notes[0].kind {
            LoadNoteKind::Invalid { found, allowed } => {
                assert_eq!(found.as_ref(), "\"nope\"");
                assert_eq!(*allowed, "целое число");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_leaf_key_is_reported() {
        let t = table("a = 1\nx = 2");
        let (_value, notes) = walk(&t, &spec());
        assert_eq!(notes, vec![LoadNote { key: KeyPath::new("x"), kind: LoadNoteKind::Unknown }]);
    }

    #[test]
    fn non_table_in_the_middle_of_path_is_absent() {
        let t = table("b = 1");
        let (value, notes) = walk(&t, &spec());
        // "b.c" не найден: "b" — не таблица; "b" сам по себе — неизвестный лист;
        // "a" отсутствует и обязателен.
        assert!(!value.b);
        assert_eq!(
            notes,
            vec![
                LoadNote { key: KeyPath::new("a"), kind: LoadNoteKind::Missing },
                LoadNote { key: KeyPath::new("b"), kind: LoadNoteKind::Unknown },
            ]
        );
    }

    #[test]
    fn notes_are_sorted_by_key_path() {
        let t = table("z = 1\nk = 2");
        let (_value, notes) = walk(&t, &spec());
        let keys: Vec<&str> = notes.iter().map(|n| n.key.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert!(notes.iter().any(|n| n.key == KeyPath::new("a")));
    }
}
