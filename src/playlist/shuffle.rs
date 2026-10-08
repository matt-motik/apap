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
