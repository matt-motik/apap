//! Модель плейлиста: идентификаторы строк и ключ сортировки (ADR-15, §3.1, ТЗ-42, ТЗ-43).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::compare::{compare_keys, CompareKeys};
use super::Track;
use crate::settings::ColumnId;

/// Стабильный идентификатор строки плейлиста в пределах сеанса, не индекс (ADR-15).
/// Выдаётся `Playlist` при добавлении; не переиспользуется (§3.1).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TrackId(u64);

/// Колонка, по которой можно сортировать: все `ColumnId`, кроме `NowPlaying` (И-Т4, ТЗ-43).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortColumn {
    TrackNumber,
    Title,
    Artist,
    Album,
    Genre,
    Year,
    Format,
    Bitrate,
    BitDepth,
    SampleRate,
    Duration,
    FileName,
    FilePath,
}

impl SortColumn {
    /// Колонка таблицы → колонка сортировки; «Сейчас играет» не сортируется (И-Т4, ТЗ-43).
    pub fn from_column(c: ColumnId) -> Option<SortColumn> {
        match c {
            ColumnId::NowPlaying => None,
            ColumnId::TrackNumber => Some(SortColumn::TrackNumber),
            ColumnId::Title => Some(SortColumn::Title),
            ColumnId::Artist => Some(SortColumn::Artist),
            ColumnId::Album => Some(SortColumn::Album),
            ColumnId::Genre => Some(SortColumn::Genre),
            ColumnId::Year => Some(SortColumn::Year),
            ColumnId::Format => Some(SortColumn::Format),
            ColumnId::Bitrate => Some(SortColumn::Bitrate),
            ColumnId::BitDepth => Some(SortColumn::BitDepth),
            ColumnId::SampleRate => Some(SortColumn::SampleRate),
            ColumnId::Duration => Some(SortColumn::Duration),
            ColumnId::FileName => Some(SortColumn::FileName),
            ColumnId::FilePath => Some(SortColumn::FilePath),
        }
    }

    /// Колонка таблицы, на которой рисуется стрелка ключа (§3.3).
    pub fn column(self) -> ColumnId {
        match self {
            SortColumn::TrackNumber => ColumnId::TrackNumber,
            SortColumn::Title => ColumnId::Title,
            SortColumn::Artist => ColumnId::Artist,
            SortColumn::Album => ColumnId::Album,
            SortColumn::Genre => ColumnId::Genre,
            SortColumn::Year => ColumnId::Year,
            SortColumn::Format => ColumnId::Format,
            SortColumn::Bitrate => ColumnId::Bitrate,
            SortColumn::BitDepth => ColumnId::BitDepth,
            SortColumn::SampleRate => ColumnId::SampleRate,
            SortColumn::Duration => ColumnId::Duration,
            SortColumn::FileName => ColumnId::FileName,
            SortColumn::FilePath => ColumnId::FilePath,
        }
    }
}

/// Направление ключа сортировки (ТЗ-42).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Ключ сортировки (§3.1, ТЗ-43). «Нет ключа» — `Option::None`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SortKey {
    pub column: SortColumn,
    pub dir: SortDir,
}

/// Строка плейлиста: трек и его ключи сравнения (§3.1).
struct Row {
    id: TrackId,
    track: Track,
    keys: CompareKeys,
}

/// Что изменила операция: для флага «плейлист изменён», `state.toml`, `Sequencer`
/// и таблицы Slint (§3.1, §6.13).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PlaylistEffect {
    /// Взвести флаг (ТЗ-12).
    pub dirty: bool,
    /// Ключ сортировки изменился (`StateChange::Sort`, `Origin::User`).
    pub sort_changed: bool,
    /// Видимый порядок изменился: передать `Sequencer`, перестроить `ShuffleState` (ТЗ-45).
    pub order_changed: bool,
}

impl PlaylistEffect {
    /// Изменение состава или исходного порядка: флаг и новый видимый порядок (§6.13).
    const EDITED: PlaylistEffect = PlaylistEffect { dirty: true, sort_changed: false, order_changed: true };
}

/// Плейлист (ADR-15): строки в исходном порядке + видимый порядок как перестановка
/// (§3.1, §3.2, И-Р14).
pub struct Playlist {
    /// Исходный порядок = порядок `playlist.m3u` (ТЗ-40).
    rows: Vec<Row>,
    /// Индекс строки в `rows` по id.
    pos: HashMap<TrackId, u32>,
    /// Видимый порядок: без ключа — порядок `rows`, с ключом — стабильная сортировка `rows`.
    visible: Vec<TrackId>,
    sort: Option<SortKey>,
    /// Следующий id; id не переиспользуются в пределах сеанса (ADR-15).
    next_id: u64,
}

impl Default for Playlist {
    fn default() -> Self {
        Playlist::new()
    }
}

impl Playlist {
    pub fn new() -> Playlist {
        Playlist { rows: Vec::new(), pos: HashMap::new(), visible: Vec::new(), sort: None, next_id: 0 }
    }

    /// Замена целиком по окончании загрузки (ADR-16): строки и уже вычисленный видимый
    /// порядок — индексы в `rows`; индексы вне диапазона пропускаются (§3.1).
    pub fn replace(&mut self, rows: Vec<(Track, CompareKeys)>, visible: Vec<u32>, sort: Option<SortKey>) {
        self.rows = rows.into_iter().map(|(track, keys)| Row { id: self.alloc_id(), track, keys }).collect();
        self.rebuild_pos();
        self.visible = visible
            .into_iter()
            .filter_map(|i| usize::try_from(i).ok().and_then(|i| self.rows.get(i)).map(|r| r.id))
            .collect();
        self.sort = sort;
    }

    /// Добавление в конец исходного порядка; в видимом — место по ключу, равные —
    /// перед новой строкой (ТЗ-41, ТЗ-45, §6.13).
    pub fn add(&mut self, tracks: Vec<(Track, CompareKeys)>) -> (Vec<TrackId>, PlaylistEffect) {
        if tracks.is_empty() {
            return (Vec::new(), PlaylistEffect::default());
        }
        let mut ids = Vec::with_capacity(tracks.len());
        for (track, keys) in tracks {
            let id = self.alloc_id();
            if let Ok(i) = u32::try_from(self.rows.len()) {
                self.pos.insert(id, i);
            }
            self.rows.push(Row { id, track, keys });
            let at = match self.sort {
                None => self.visible.len(),
                Some(key) => self.visible.partition_point(|&v| self.cmp_ids(v, id, key) != Ordering::Greater),
            };
            self.visible.insert(at, id);
            ids.push(id);
        }
        (ids, PlaylistEffect::EDITED)
    }

    /// Удаление строк из обоих порядков; неизвестные id пропускаются (§6.13).
    pub fn remove(&mut self, ids: &[TrackId]) -> PlaylistEffect {
        let gone: HashSet<TrackId> = ids.iter().copied().filter(|id| self.pos.contains_key(id)).collect();
        if gone.is_empty() {
            return PlaylistEffect::default();
        }
        self.rows.retain(|r| !gone.contains(&r.id));
        self.visible.retain(|id| !gone.contains(id));
        self.rebuild_pos();
        PlaylistEffect::EDITED
    }

    /// Очистка плейлиста; ключ сортировки сохраняется (§6.13).
    pub fn clear(&mut self) -> PlaylistEffect {
        if self.rows.is_empty() {
            return PlaylistEffect::default();
        }
        self.rows.clear();
        self.pos.clear();
        self.visible.clear();
        PlaylistEffect::EDITED
    }

    /// Щелчок по заголовку `c` (ТЗ-42, ТЗ-43, §3.3): нет ключа или другая колонка → `↑`,
    /// `↑` → `↓`, `↓` → нет ключа; «Сейчас играет» не сортируется. `hidden` — колонка
    /// скрыта (ТЗ-31): заголовка нет, щелчок не меняет ключ. Флаг не взводится (ТЗ-40).
    pub fn header_click(&mut self, c: ColumnId, hidden: bool) -> PlaylistEffect {
        let Some(column) = SortColumn::from_column(c) else {
            return PlaylistEffect::default();
        };
        if hidden {
            return PlaylistEffect::default();
        }
        self.sort = match self.sort {
            Some(SortKey { column: cur, dir: SortDir::Asc }) if cur == column => {
                Some(SortKey { column, dir: SortDir::Desc })
            }
            Some(SortKey { column: cur, dir: SortDir::Desc }) if cur == column => None,
            _ => Some(SortKey { column, dir: SortDir::Asc }),
        };
        self.resort();
        PlaylistEffect { dirty: false, sort_changed: true, order_changed: true }
    }

    /// Ручное изменение порядка (ТЗ-44, §6.13): `moved` — в видимом порядке, `before` —
    /// строка, перед которой вставить (`None` или перемещаемая — в конец). Новый видимый
    /// порядок становится исходным, ключ сортировки снимается.
    pub fn reorder(&mut self, moved: &[TrackId], before: Option<TrackId>) -> PlaylistEffect {
        let moving: HashSet<TrackId> = moved.iter().copied().filter(|id| self.pos.contains_key(id)).collect();
        if moving.is_empty() {
            return PlaylistEffect::default();
        }
        let (block, mut vis): (Vec<TrackId>, Vec<TrackId>) =
            self.visible.iter().copied().partition(|id| moving.contains(id));
        let at = before.and_then(|b| vis.iter().position(|&v| v == b)).unwrap_or(vis.len());
        vis.splice(at..at, block);

        let mut by_id: HashMap<TrackId, Row> = self.rows.drain(..).map(|r| (r.id, r)).collect();
        self.rows = vis.iter().filter_map(|id| by_id.remove(id)).collect();
        // Строки вне видимого порядка (И-Р14 нарушен) не теряются: в конец.
        let mut rest: Vec<Row> = by_id.into_values().collect();
        rest.sort_by_key(|r| r.id);
        self.rows.extend(rest);
        self.rebuild_pos();
        self.visible = self.rows.iter().map(|r| r.id).collect();

        let sort_changed = self.sort.take().is_some();
        PlaylistEffect { dirty: true, sort_changed, order_changed: true }
    }

    /// Обновление тегов строки: пересчёт ключей; видимый порядок не меняется до
    /// следующей сортировки (§3.1, ТЗ-43).
    pub fn update_tags(&mut self, id: TrackId, track: Track, keys: CompareKeys) {
        let Some(i) = self.pos.get(&id).and_then(|&i| usize::try_from(i).ok()) else {
            return;
        };
        if let Some(row) = self.rows.get_mut(i) {
            row.track = track;
            row.keys = keys;
        }
    }

    /// Треки в исходном порядке — порядок `playlist.m3u` (ТЗ-40, §3.2).
    pub fn source_order(&self) -> impl Iterator<Item = &Track> {
        self.rows.iter().map(|r| &r.track)
    }

    /// Видимый порядок — порядок таблицы и `Sequencer` (§3.2).
    pub fn visible(&self) -> &[TrackId] {
        &self.visible
    }

    pub fn sort_key(&self) -> Option<SortKey> {
        self.sort
    }

    pub fn get(&self, id: TrackId) -> Option<&Track> {
        self.row(id).map(|r| &r.track)
    }

    /// Первый трек видимого порядка: «Далее» без текущего и без Shuffle (ТЗ-46).
    pub fn first_visible(&self) -> Option<TrackId> {
        self.visible.first().copied()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Позиция строки в видимом порядке (строка таблицы).
    pub fn index_of(&self, id: TrackId) -> Option<usize> {
        self.visible.iter().position(|&v| v == id)
    }

    /// Видимый порядок заново из исходного: id в порядке `rows`, затем стабильная
    /// сортировка по ключу (§3.2, §6.13, И-Р14).
    fn resort(&mut self) {
        let mut visible: Vec<TrackId> = self.rows.iter().map(|r| r.id).collect();
        if let Some(key) = self.sort {
            visible.sort_by(|&a, &b| self.cmp_ids(a, b, key));
        }
        self.visible = visible;
    }

    fn alloc_id(&mut self) -> TrackId {
        let id = TrackId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn rebuild_pos(&mut self) {
        self.pos = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| u32::try_from(i).ok().map(|i| (r.id, i)))
            .collect();
    }

    fn row(&self, id: TrackId) -> Option<&Row> {
        let i = usize::try_from(*self.pos.get(&id)?).ok()?;
        self.rows.get(i)
    }

    /// `cmp(a, b)` §6.13: ключ сравнения, затем исходный индекс — стабильность (И-Р14).
    fn cmp_ids(&self, a: TrackId, b: TrackId, key: SortKey) -> Ordering {
        let by_key = match (self.row(a), self.row(b)) {
            (Some(ra), Some(rb)) => compare_keys(&ra.keys, &rb.keys, key),
            _ => Ordering::Equal,
        };
        by_key.then_with(|| self.pos.get(&a).cmp(&self.pos.get(&b)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, year: &str) -> (Track, CompareKeys) {
        let t = Track { title: title.into(), year: year.into(), ..Track::default() };
        let keys = CompareKeys::from_track(&t);
        (t, keys)
    }

    fn playlist(titles: &[&str]) -> Playlist {
        let mut p = Playlist::new();
        p.add(titles.iter().map(|t| track(t, "")).collect());
        p
    }

    fn visible_titles(p: &Playlist) -> Vec<String> {
        p.visible().iter().filter_map(|&id| p.get(id)).map(|t| t.title.clone()).collect()
    }

    fn source_titles(p: &Playlist) -> Vec<String> {
        p.source_order().map(|t| t.title.clone()).collect()
    }

    fn key(column: SortColumn, dir: SortDir) -> Option<SortKey> {
        Some(SortKey { column, dir })
    }

    const RESORTED: PlaylistEffect = PlaylistEffect { dirty: false, sort_changed: true, order_changed: true };

    /// «Название ↑», добавить «A…»: первая в видимом, последняя в исходном (ТЗ-41, ТЗ-45).
    #[test]
    fn added_track_takes_place_by_key() {
        let mut p = playlist(&["B", "C"]);
        p.header_click(ColumnId::Title, false);
        let (ids, effect) = p.add(vec![track("A first", "")]);
        assert_eq!(effect, PlaylistEffect::EDITED);
        assert_eq!(p.first_visible(), ids.first().copied());
        assert_eq!(visible_titles(&p), ["A first", "B", "C"]);
        assert_eq!(source_titles(&p), ["B", "C", "A first"]);
    }

    /// Равный ключ — после равных: стабильность по исходному индексу (ТЗ-41, И-Р14).
    #[test]
    fn added_equal_key_goes_after_equals() {
        let mut p = Playlist::new();
        p.add(vec![track("X", "1"), track("Y", "")]);
        p.header_click(ColumnId::Title, false);
        let (ids, _) = p.add(vec![track("x", "2")]);
        let years: Vec<String> = p.visible().iter().filter_map(|&id| p.get(id)).map(|t| t.year.clone()).collect();
        assert_eq!(years, ["1", "2", ""]);
        assert_eq!(p.index_of(ids[0]), Some(1));
    }

    /// Три щелчка «Название»: ↑, ↓, нет; после третьего видимый = исходный; флаг не
    /// взводится (ТЗ-40, ТЗ-42).
    #[test]
    fn header_click_cycles_asc_desc_none() {
        let mut p = playlist(&["C", "A", "B"]);
        assert_eq!(p.header_click(ColumnId::Title, false), RESORTED);
        assert_eq!(p.sort_key(), key(SortColumn::Title, SortDir::Asc));
        assert_eq!(visible_titles(&p), ["A", "B", "C"]);
        assert_eq!(p.header_click(ColumnId::Title, false), RESORTED);
        assert_eq!(p.sort_key(), key(SortColumn::Title, SortDir::Desc));
        assert_eq!(visible_titles(&p), ["C", "B", "A"]);
        assert_eq!(p.header_click(ColumnId::Title, false), RESORTED);
        assert_eq!(p.sort_key(), None);
        assert_eq!(visible_titles(&p), source_titles(&p));
        assert_eq!(source_titles(&p), ["C", "A", "B"]);
    }

    /// «Название ↓», щелчок «Год» → «Год ↑» (ТЗ-42).
    #[test]
    fn other_column_click_starts_asc() {
        let mut p = playlist(&["A", "B"]);
        p.header_click(ColumnId::Title, false);
        p.header_click(ColumnId::Title, false);
        assert_eq!(p.sort_key(), key(SortColumn::Title, SortDir::Desc));
        assert_eq!(p.header_click(ColumnId::Year, false), RESORTED);
        assert_eq!(p.sort_key(), key(SortColumn::Year, SortDir::Asc));
    }

    /// «Сейчас играет» не сортируется: ключ и порядок без изменений (ТЗ-43, И-Т4).
    #[test]
    fn now_playing_column_not_sortable() {
        let mut p = playlist(&["B", "A"]);
        p.header_click(ColumnId::Title, false);
        assert_eq!(p.header_click(ColumnId::NowPlaying, false), PlaylistEffect::default());
        assert_eq!(p.sort_key(), key(SortColumn::Title, SortDir::Asc));
        assert_eq!(visible_titles(&p), ["A", "B"]);
    }

    /// «Название ↑»; 3-я строка на 1-е место: ключ снят, видимый = исходный =
    /// отсортированный с перемещённой строкой, флаг взведён (ТЗ-44).
    #[test]
    fn drag_reorder_clears_sort_key() {
        let mut p = playlist(&["C", "A", "D", "B"]);
        p.header_click(ColumnId::Title, false);
        let moved = p.visible()[2];
        let before = p.first_visible();
        let effect = p.reorder(&[moved], before);
        assert_eq!(effect, PlaylistEffect { dirty: true, sort_changed: true, order_changed: true });
        assert_eq!(p.sort_key(), None);
        assert_eq!(visible_titles(&p), ["C", "A", "B", "D"]);
        assert_eq!(source_titles(&p), visible_titles(&p));
        // Перенос в конец без ключа: ключ уже снят, sort_changed не повторяется.
        let first = p.visible()[0];
        let effect = p.reorder(&[first], None);
        assert_eq!(effect, PlaylistEffect::EDITED);
        assert_eq!(source_titles(&p), ["A", "B", "D", "C"]);
    }

    /// Скрытая колонка ключа: щелчок по ней ничего не меняет, ключ и порядок
    /// сохраняются (ТЗ-31, §3.3).
    #[test]
    fn hide_sorted_column_keeps_order() {
        let mut p = Playlist::new();
        p.add(vec![track("A", "1990"), track("B", "2001"), track("C", "1969")]);
        p.header_click(ColumnId::Year, false);
        p.header_click(ColumnId::Year, false);
        assert_eq!(visible_titles(&p), ["B", "A", "C"]);
        assert_eq!(p.header_click(ColumnId::Year, true), PlaylistEffect::default());
        assert_eq!(p.sort_key(), key(SortColumn::Year, SortDir::Desc));
        assert_eq!(visible_titles(&p), ["B", "A", "C"]);
    }

    /// Новые теги не переставляют строку до следующей сортировки (§3.1).
    #[test]
    fn update_tags_keeps_visible_order_until_resort() {
        let mut p = playlist(&["A", "B"]);
        p.header_click(ColumnId::Title, false);
        let a = p.visible()[0];
        let (t, k) = track("Z", "");
        p.update_tags(a, t, k);
        assert_eq!(visible_titles(&p), ["Z", "B"]);
        p.header_click(ColumnId::Title, false);
        p.header_click(ColumnId::Title, false);
        p.header_click(ColumnId::Title, false);
        assert_eq!(visible_titles(&p), ["B", "Z"]);
    }

    /// Удаление и очистка: оба порядка, флаг; ключ сохраняется (§6.13).
    #[test]
    fn remove_and_clear_update_both_orders() {
        let mut p = playlist(&["C", "A", "B"]);
        p.header_click(ColumnId::Title, false);
        let a = p.visible()[0];
        assert_eq!(p.remove(&[a]), PlaylistEffect::EDITED);
        assert!(p.get(a).is_none());
        assert_eq!(visible_titles(&p), ["B", "C"]);
        assert_eq!(source_titles(&p), ["C", "B"]);
        assert_eq!(p.remove(&[a]), PlaylistEffect::default());
        assert_eq!(p.clear(), PlaylistEffect::EDITED);
        assert!(p.is_empty());
        assert_eq!(p.first_visible(), None);
        assert_eq!(p.sort_key(), key(SortColumn::Title, SortDir::Asc));
    }
}
