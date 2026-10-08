//! Проход Shuffle по видимому порядку плейлиста (ADR-15, §3.4, ТЗ-45, ТЗ-46).

use std::collections::HashSet;

use rand::seq::SliceRandom;
use rand::Rng;

use super::model::TrackId;
use crate::settings::RepeatMode;

/// Проход Shuffle (ТЗ-45, ТЗ-46): сыгранное позади курсора, несыгранное — впереди
/// в случайном порядке; каждый трек прохода играет один раз (§3.4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShuffleState {
    /// Сыгранные в текущем проходе, по порядку.
    history: Vec<TrackId>,
    current: Option<TrackId>,
    /// Несыгранные, случайный порядок.
    upcoming: Vec<TrackId>,
}

impl ShuffleState {
    /// Новый проход: всё видимое — несыгранное, в случайном порядке (§3.4).
    pub fn new_pass(visible: &[TrackId], rng: &mut impl Rng) -> ShuffleState {
        let mut upcoming = visible.to_vec();
        upcoming.shuffle(rng);
        ShuffleState { history: Vec::new(), current: None, upcoming }
    }

    /// Перестроение после изменения видимого порядка или состава (§3.4, ТЗ-45): исчезнувшие
    /// id удаляются, `history` и `current` сохраняются, новые id попадают в `upcoming`,
    /// `upcoming` перемешивается заново. Удалённый текущий трек перестаёт быть текущим.
    pub fn rebuild(&mut self, visible: &[TrackId], rng: &mut impl Rng) {
        let present: HashSet<TrackId> = visible.iter().copied().collect();
        self.history.retain(|id| present.contains(id));
        self.upcoming.retain(|id| present.contains(id));
        if self.current.is_some_and(|c| !present.contains(&c)) {
            self.current = None;
        }
        let known: HashSet<TrackId> =
            self.history.iter().chain(self.current.iter()).chain(self.upcoming.iter()).copied().collect();
        self.upcoming.extend(visible.iter().copied().filter(|id| !known.contains(id)));
        self.upcoming.shuffle(rng);
    }

    /// Перестановка для `Sequencer`: `history ++ [current] ++ upcoming` (§3.4).
    pub fn order(&self) -> Vec<TrackId> {
        self.history.iter().chain(self.current.iter()).chain(self.upcoming.iter()).copied().collect()
    }

    /// Начато воспроизведение `id`: прежний текущий уходит в `history`, `id` убирается
    /// из несыгранных (§3.4).
    pub fn started(&mut self, id: TrackId) {
        if self.current == Some(id) {
            return;
        }
        if let Some(prev) = self.current.take() {
            self.history.push(prev);
        }
        self.history.retain(|&h| h != id);
        self.upcoming.retain(|&u| u != id);
        self.current = Some(id);
    }

    /// Первый несыгранный трек прохода — «Далее» без текущего трека и следующий трек
    /// после текущего (ТЗ-46, Т-ТЗ-46, §3.4). Несыгранных нет: Repeat All — новый проход
    /// (`history` очищается, перестановка видимого без текущего) и его первый трек;
    /// Repeat Off и Repeat One — `None`, стоп без сообщения (ТЗ-45).
    pub fn first(&mut self, visible: &[TrackId], repeat: RepeatMode, rng: &mut impl Rng) -> Option<TrackId> {
        if let Some(&next) = self.upcoming.first() {
            return Some(next);
        }
        if repeat != RepeatMode::All {
            return None;
        }
        self.history.clear();
        self.upcoming = visible.iter().copied().filter(|&id| Some(id) != self.current).collect();
        self.upcoming.shuffle(rng);
        self.upcoming.first().copied()
    }

    /// Последний сыгранный трек прохода — «Назад» при Shuffle.
    pub fn previous(&self) -> Option<TrackId> {
        self.history.last().copied()
    }

    /// «Назад» при Shuffle (§3.4, ТЗ-45): последний трек `history` становится
    /// текущим, прежний текущий (если был) встаёт в начало `upcoming`, чтобы
    /// последующее «Далее» снова проиграло трек, с которого ушли назад.
    /// `history` пуста — `None`, состояние не меняется.
    pub fn back(&mut self) -> Option<TrackId> {
        let prev = self.history.pop()?;
        if let Some(cur) = self.current.take() {
            self.upcoming.insert(0, cur);
        }
        self.current = Some(prev);
        Some(prev)
    }

    pub fn current(&self) -> Option<TrackId> {
        self.current
    }

    pub fn history(&self) -> &[TrackId] {
        &self.history
    }

    pub fn upcoming(&self) -> &[TrackId] {
        &self.upcoming
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    use super::super::compare::CompareKeys;
    use super::super::model::Playlist;
    use super::super::Track;
    use super::*;
    use crate::settings::ColumnId;

    fn track(i: usize) -> (Track, CompareKeys) {
        let t = Track { title: format!("{i}"), ..Track::default() };
        let k = CompareKeys::from_track(&t);
        (t, k)
    }

    fn playlist(n: usize) -> Playlist {
        let mut p = Playlist::new();
        p.add((0..n).map(track).collect());
        p
    }

    /// Имитация «Далее»: первый несыгранный проходом трек + отметка его начатым
    /// (ТЗ-46, Т-ТЗ-46, §3.4).
    fn play_next(s: &mut ShuffleState, vis: &[TrackId], r: RepeatMode, rng: &mut impl Rng) -> Option<TrackId> {
        let id = s.first(vis, r, rng)?;
        s.started(id);
        Some(id)
    }

    /// Смена сортировки и добавление треков во время прохода Shuffle сохраняют
    /// `current`; каждый трек прохода играет ровно один раз (ТЗ-45, §3.4).
    #[test]
    fn shuffle_changes_keep_pass() {
        let mut rng = StdRng::seed_from_u64(1);
        let mut p = playlist(10);
        let mut s = ShuffleState::new_pass(p.visible(), &mut rng);

        let mut played = Vec::new();
        for _ in 0..4 {
            let id = play_next(&mut s, p.visible(), RepeatMode::Off, &mut rng).expect("трек должен быть");
            played.push(id);
        }
        let current = s.current();

        p.header_click(ColumnId::Title, false);
        s.rebuild(p.visible(), &mut rng);
        assert_eq!(s.current(), current);

        p.add(vec![track(10), track(11)]);
        s.rebuild(p.visible(), &mut rng);
        assert_eq!(s.current(), current);

        while let Some(id) = play_next(&mut s, p.visible(), RepeatMode::Off, &mut rng) {
            played.push(id);
        }

        assert_eq!(played.len(), 12);
        let unique: HashSet<TrackId> = played.iter().copied().collect();
        assert_eq!(unique.len(), 12);
    }

    /// Новый проход: «Далее» без текущего трека — первый из `order`/`upcoming`,
    /// история пуста (ТЗ-46, Т-ТЗ-46, §3.4).
    #[test]
    fn next_without_current_shuffle_pass_start() {
        let mut rng = StdRng::seed_from_u64(2);
        let p = playlist(5);
        let mut s = ShuffleState::new_pass(p.visible(), &mut rng);

        let first = s.first(p.visible(), RepeatMode::Off, &mut rng);
        assert_eq!(first, s.order().first().copied());
        assert_eq!(first, s.upcoming().first().copied());
        assert!(s.history().is_empty());
    }

    /// Текущий трек удалён и состояние перестроено без текущего: «Далее» — первый
    /// несыгранный, не из уже сыгранных (ТЗ-46, Т-ТЗ-46, §3.4).
    #[test]
    fn next_without_current_shuffle_mid_pass() {
        let mut rng = StdRng::seed_from_u64(3);
        let mut p = playlist(6);
        let mut s = ShuffleState::new_pass(p.visible(), &mut rng);

        let mut played = HashSet::new();
        for _ in 0..3 {
            let id = play_next(&mut s, p.visible(), RepeatMode::Off, &mut rng).expect("трек должен быть");
            played.insert(id);
        }
        let current = s.current().expect("после play_next есть текущий");
        p.remove(&[current]);
        s.rebuild(p.visible(), &mut rng);
        assert_eq!(s.current(), None);

        let first = s.first(p.visible(), RepeatMode::Off, &mut rng).expect("есть несыгранные");
        assert!(!played.contains(&first));
        assert_eq!(Some(first), s.upcoming().first().copied());
    }

    /// Все треки прохода сыграны, текущий удалён и состояние перестроено без
    /// текущего: без Repeat All — `None`, и при Off, и при One (ТЗ-45, ТЗ-46, §3.4).
    #[test]
    fn next_without_current_shuffle_all_played_repeat_off() {
        let mut rng = StdRng::seed_from_u64(4);
        let mut p = playlist(6);
        let mut s = ShuffleState::new_pass(p.visible(), &mut rng);

        for _ in 0..6 {
            play_next(&mut s, p.visible(), RepeatMode::Off, &mut rng).expect("трек должен быть");
        }
        let current = s.current().expect("после play_next есть текущий");
        p.remove(&[current]);
        s.rebuild(p.visible(), &mut rng);
        assert_eq!(s.current(), None);

        assert_eq!(s.first(p.visible(), RepeatMode::Off, &mut rng), None);
        assert_eq!(s.first(p.visible(), RepeatMode::One, &mut rng), None);
    }

    /// Все треки прохода сыграны, текущий удалён и состояние перестроено без
    /// текущего: Repeat All — новый проход (история пуста), «Далее» — первый трек
    /// перестановки (ТЗ-45, ТЗ-46, §3.4).
    #[test]
    fn next_without_current_shuffle_all_played_repeat_all() {
        let mut rng = StdRng::seed_from_u64(5);
        let mut p = playlist(6);
        let mut s = ShuffleState::new_pass(p.visible(), &mut rng);

        for _ in 0..6 {
            play_next(&mut s, p.visible(), RepeatMode::All, &mut rng).expect("трек должен быть");
        }
        let current = s.current().expect("после play_next есть текущий");
        p.remove(&[current]);
        s.rebuild(p.visible(), &mut rng);
        assert_eq!(s.current(), None);

        let first = s.first(p.visible(), RepeatMode::All, &mut rng).expect("Repeat All начинает новый проход");
        assert!(s.history().is_empty());
        assert_eq!(s.upcoming().len(), 5);
        assert_eq!(Some(first), s.upcoming().first().copied());
    }

    /// `started` переносит прежний текущий в `history`; `order` — `history ++
    /// current ++ upcoming` (ТЗ-45, §3.4).
    #[test]
    fn started_moves_previous_to_history() {
        let mut rng = StdRng::seed_from_u64(6);
        let p = playlist(4);
        let mut s = ShuffleState::new_pass(p.visible(), &mut rng);
        let rest = s.upcoming()[2..].to_vec();
        let a = s.upcoming()[0];
        let b = s.upcoming()[1];

        s.started(a);
        s.started(b);

        assert_eq!(s.history(), [a]);
        assert_eq!(s.previous(), Some(a));
        let mut expected = vec![a, b];
        expected.extend(rest);
        assert_eq!(s.order(), expected);
    }

    /// «Назад» дважды подряд идёт по `history`, а не колеблется между двумя
    /// треками: a -> b -> c, назад -> b, назад -> a, назад -> `None` без
    /// изменения состояния (ТЗ-45, §3.4).
    #[test]
    fn back_twice_walks_history() {
        let mut rng = StdRng::seed_from_u64(7);
        let p = playlist(4);
        let mut s = ShuffleState::new_pass(p.visible(), &mut rng);
        let a = s.upcoming()[0];
        let b = s.upcoming()[1];
        let c = s.upcoming()[2];

        s.started(a);
        s.started(b);
        s.started(c);

        assert_eq!(s.back(), Some(b));
        assert_eq!(s.back(), Some(a));

        let before = s.clone();
        assert_eq!(s.back(), None);
        assert_eq!(s, before);
    }

    /// После «Назад» с c на b следующее «Далее» снова играет c — трек, с
    /// которого ушли назад (ТЗ-45, §3.4).
    #[test]
    fn next_after_back_replays_left_track() {
        let mut rng = StdRng::seed_from_u64(8);
        let p = playlist(4);
        let mut s = ShuffleState::new_pass(p.visible(), &mut rng);
        let a = s.upcoming()[0];
        let b = s.upcoming()[1];
        let c = s.upcoming()[2];

        s.started(a);
        s.started(b);
        s.started(c);

        assert_eq!(s.back(), Some(b));
        assert_eq!(s.first(p.visible(), RepeatMode::Off, &mut rng), Some(c));
    }
}
