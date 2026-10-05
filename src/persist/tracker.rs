//! Моменты записи (ADR-3, §2.7): какие рабочие файлы пора писать и когда.
//! UI-поток; ввода-вывода не выполняет (ТЗ-3) — только решает, когда и какой
//! снимок отправить писателю `apap-persist` (`writer::WriterHandle`).

use super::settings_file::SaveInterval;
use super::state_file::Origin;
use super::writer::WriterReply;
use super::{ConfigFile, ReferenceText, Snapshot, SnapshotId, WorkFile};
use crate::audio::clock::ClockInstant;
use crate::platform::fs::WriteErrorClass;
use std::collections::VecDeque;
use std::sync::Arc;

/// Моменты записи (ADR-3). UI-поток; ввода-вывода не выполняет (§2.7).
#[derive(Debug)]
pub struct PersistTracker {
    interval: SaveInterval,
    /// Срок отложенной записи; общий для `state.toml` и `playlist.m3u` (ТЗ-11).
    deadline: Option<ClockInstant>,
    settings: FileTrack,
    state: FileTrack,
    playlist: PlaylistTrack,
    next_snapshot: u64,
}

/// Учёт одного TOML-файла (§2.7).
#[derive(Debug)]
struct FileTrack {
    reference: ReferenceText,
    /// Запись по отсчёту остановлена ошибкой до успешного «Повторить» (ТЗ-20).
    stopped: bool,
    /// Файл существовал, но не прочитан при запуске (ОВС-6 в): запись по
    /// отсчёту и на пути выхода запрещена до конца сеанса; «Сохранить» и
    /// «Повторить» (явные действия) пишут.
    auto_forbidden: bool,
    /// Отправленные без ответа снимки в порядке отправки; не больше двух —
    /// исполняемый и ожидающий в слоте (И-Р1).
    in_flight: VecDeque<(SnapshotId, Arc<[u8]>)>,
}

impl FileTrack {
    fn new(reference: ReferenceText) -> FileTrack {
        FileTrack { reference, stopped: false, auto_forbidden: false, in_flight: VecDeque::new() }
    }
}

/// Учёт `playlist.m3u` (§2.7). Флаг «плейлист изменён» (ТЗ-12) =
/// `change_seq > written_seq`.
#[derive(Debug)]
struct PlaylistTrack {
    /// Номер последнего изменения, взводящего флаг.
    change_seq: u64,
    /// Номер изменения, вошедшего в последнюю успешную запись.
    written_seq: u64,
    stopped: bool,
    /// Случай 3 ОВ-8: запись запрещена до конца сеанса (ТЗ-12, ТЗ-21).
    forbidden: bool,
    in_flight: VecDeque<(SnapshotId, u64)>,
}

impl PlaylistTrack {
    fn new() -> PlaylistTrack {
        PlaylistTrack { change_seq: 0, written_seq: 0, stopped: false, forbidden: false, in_flight: VecDeque::new() }
    }
}

/// Какие файлы пора записать по сроку (ТЗ-11, ТЗ-12, §6.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DueFiles {
    pub state: bool,
    pub playlist: bool,
}

/// Последствие ответа писателя для окна ошибок (ADR-13, §6.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplyEffect {
    None,
    Succeeded(WorkFile),
    Failed(WorkFile, WriteErrorClass),
}

impl PersistTracker {
    pub fn new(interval: SaveInterval, settings_ref: ReferenceText, state_ref: ReferenceText) -> PersistTracker {
        PersistTracker {
            interval,
            deadline: None,
            settings: FileTrack::new(settings_ref),
            state: FileTrack::new(state_ref),
            playlist: PlaylistTrack::new(),
            next_snapshot: 0,
        }
    }

    /// Изменение состояния сессии (§6.4): `User` без идущего срока → срок
    /// `:= now + N`; `Program` — ничего (ADR-3, ADR-22).
    pub fn on_state_changed(&mut self, origin: Origin, now: ClockInstant) {
        if origin == Origin::User && self.deadline.is_none() {
            self.deadline = Some(now.saturating_add(self.interval.duration()));
        }
    }

    /// Изменение плейлиста, взводящее флаг (ТЗ-12, §6.4): `change_seq += 1`;
    /// срок — как у `User`-изменения.
    pub fn on_playlist_changed(&mut self, now: ClockInstant) {
        self.playlist.change_seq += 1;
        if self.deadline.is_none() {
            self.deadline = Some(now.saturating_add(self.interval.duration()));
        }
    }

    /// Случай 3 ОВ-8: запись `playlist.m3u` запрещена до конца сеанса (ТЗ-12, ТЗ-21).
    pub fn forbid_playlist(&mut self) {
        self.playlist.forbidden = true;
    }

    /// ОВС-6 в: запрет автоматических записей TOML-файла `f` до конца сеанса.
    pub fn forbid_auto(&mut self, f: ConfigFile) {
        self.track_mut(f).auto_forbidden = true;
    }

    pub fn auto_allowed(&self, f: ConfigFile) -> bool {
        !self.track(f).auto_forbidden
    }

    /// Истёк ли срок (§6.4); если да — снимает его и возвращает
    /// файлы-кандидаты. `state` — всегда `true` при истечении срока (отличие
    /// от эталона проверяется отдельно сравнением текста, `toml_needs_write`);
    /// `playlist` — только если флаг «изменён» взведён.
    pub fn poll_deadline(&mut self, now: ClockInstant) -> DueFiles {
        match self.deadline {
            Some(d) if now >= d => {
                self.deadline = None;
                DueFiles { state: true, playlist: self.playlist_dirty() }
            }
            _ => DueFiles::default(),
        }
    }

    /// Смена `N` по «Сохранить» (§6.4, У-1): срок `:= min(срок, now + N_new)`, если срок идёт.
    pub fn set_interval(&mut self, n: SaveInterval, now: ClockInstant) {
        self.interval = n;
        if let Some(d) = self.deadline {
            self.deadline = Some(d.min(now.saturating_add(n.duration())));
        }
    }

    /// Нужно ли писать TOML-файл `f` с текстом `text` (ОВ-2): текст отличается
    /// от последнего отправленного, а если отправленных нет — от эталона.
    pub fn toml_needs_write(&self, f: ConfigFile, text: &[u8]) -> bool {
        let track = self.track(f);
        match track.in_flight.back() {
            Some((_, bytes)) => bytes.as_ref() != text,
            None => track.reference.differs(text),
        }
    }

    pub fn playlist_dirty(&self) -> bool {
        self.playlist.change_seq > self.playlist.written_seq
    }

    /// `forbidden` блокирует запись навсегда; `stopped` блокирует только
    /// запись по отсчёту (`by_timer`) — явное «Повторить»/«Сохранить» пишет
    /// независимо от прежней неудачи (ТЗ-20).
    pub fn playlist_writable(&self, by_timer: bool) -> bool {
        if self.playlist.forbidden {
            return false;
        }
        !(by_timer && self.playlist.stopped)
    }

    /// Следующий монотонный идентификатор снимка; общий для всех трёх
    /// рабочих файлов (§2.7).
    pub fn next_id(&mut self) -> SnapshotId {
        let id = SnapshotId::new(self.next_snapshot);
        self.next_snapshot += 1;
        id
    }

    /// Учесть отправленный снимок; для плейлиста — номер изменения, который
    /// он содержит (§6.5).
    pub fn sent(&mut self, snap: &Snapshot, playlist_seq: Option<u64>) {
        match snap.file {
            WorkFile::Settings => self.settings.in_flight.push_back((snap.id, Arc::clone(&snap.bytes))),
            WorkFile::State => self.state.in_flight.push_back((snap.id, Arc::clone(&snap.bytes))),
            WorkFile::Playlist => {
                let seq = playlist_seq.unwrap_or(self.playlist.change_seq);
                self.playlist.in_flight.push_back((snap.id, seq));
            }
        }
    }

    /// Учесть ответ писателя: эталон, флаг, остановка (§6.5).
    pub fn on_reply(&mut self, reply: &WriterReply) -> ReplyEffect {
        match reply {
            WriterReply::Written { file, id } => self.on_written(*file, *id),
            WriterReply::Failed { file, id, err } => self.on_failed(*file, *id, err.class),
            WriterReply::Superseded { file, id } => {
                self.remove_in_flight(*file, *id);
                ReplyEffect::None
            }
            _ => ReplyEffect::None,
        }
    }

    pub fn interval(&self) -> SaveInterval {
        self.interval
    }

    pub fn deadline(&self) -> Option<ClockInstant> {
        self.deadline
    }

    pub fn playlist_forbidden(&self) -> bool {
        self.playlist.forbidden
    }

    /// Остановлена ли запись по отсчёту для файла `f` ошибкой (ТЗ-20).
    pub fn timer_stopped(&self, f: WorkFile) -> bool {
        match f {
            WorkFile::Playlist => self.playlist.stopped,
            WorkFile::State => self.state.stopped,
            WorkFile::Settings => self.settings.stopped,
        }
    }

    /// Идентификатор последнего отправленного без ответа снимка файла `f`, если есть.
    pub fn last_in_flight(&self, f: WorkFile) -> Option<SnapshotId> {
        match f {
            WorkFile::Playlist => self.playlist.in_flight.back().map(|(id, _)| *id),
            WorkFile::State => self.state.in_flight.back().map(|(id, _)| *id),
            WorkFile::Settings => self.settings.in_flight.back().map(|(id, _)| *id),
        }
    }

    fn track(&self, f: ConfigFile) -> &FileTrack {
        match f {
            ConfigFile::Settings => &self.settings,
            ConfigFile::State => &self.state,
        }
    }

    fn track_mut(&mut self, f: ConfigFile) -> &mut FileTrack {
        match f {
            ConfigFile::Settings => &mut self.settings,
            ConfigFile::State => &mut self.state,
        }
    }

    /// `Written{f, id}` (§6.5): более ранние в полёте — уже обработанные или
    /// `Superseded`, отбрасываются вместе с найденным `id`; эталон/`written_seq`
    /// обновляются из его содержимого, остановка снимается независимо от файла.
    fn on_written(&mut self, file: WorkFile, id: SnapshotId) -> ReplyEffect {
        match file {
            WorkFile::Playlist => {
                if let Some(seq) = pop_front_until(&mut self.playlist.in_flight, id) {
                    self.playlist.written_seq = self.playlist.written_seq.max(seq);
                }
                self.playlist.stopped = false;
            }
            WorkFile::State => Self::apply_written(&mut self.state, id),
            WorkFile::Settings => Self::apply_written(&mut self.settings, id),
        }
        ReplyEffect::Succeeded(file)
    }

    fn apply_written(track: &mut FileTrack, id: SnapshotId) {
        if let Some(bytes) = pop_front_until(&mut track.in_flight, id) {
            track.reference = ReferenceText::of(bytes);
        }
        track.stopped = false;
    }

    /// `Failed{f, id, err}` (§6.5): эталон и флаг не меняются (ТЗ-20).
    fn on_failed(&mut self, file: WorkFile, id: SnapshotId, class: WriteErrorClass) -> ReplyEffect {
        match file {
            WorkFile::Playlist => {
                remove_id(&mut self.playlist.in_flight, id);
                self.playlist.stopped = true;
            }
            WorkFile::State => {
                remove_id(&mut self.state.in_flight, id);
                self.state.stopped = true;
            }
            WorkFile::Settings => {
                remove_id(&mut self.settings.in_flight, id);
                self.settings.stopped = true;
            }
        }
        ReplyEffect::Failed(file, class)
    }

    fn remove_in_flight(&mut self, file: WorkFile, id: SnapshotId) {
        match file {
            WorkFile::Playlist => remove_id(&mut self.playlist.in_flight, id),
            WorkFile::State => remove_id(&mut self.state.in_flight, id),
            WorkFile::Settings => remove_id(&mut self.settings.in_flight, id),
        }
    }
}

/// Снять с фронта очереди все записи до `id` включительно; записи старше
/// `id` — уже обработанные или `Superseded` (§6.5). Возвращает полезную
/// нагрузку найденной записи `id`, если она была в очереди.
fn pop_front_until<T>(q: &mut VecDeque<(SnapshotId, T)>, id: SnapshotId) -> Option<T> {
    loop {
        let front = q.front()?.0;
        if front == id {
            return q.pop_front().map(|(_, v)| v);
        }
        if front < id {
            q.pop_front();
            continue;
        }
        return None;
    }
}

/// Удалить запись `id` из очереди вне зависимости от её позиции (§6.5, `Failed`/`Superseded`).
fn remove_id<T>(q: &mut VecDeque<(SnapshotId, T)>, id: SnapshotId) {
    if let Some(pos) = q.iter().position(|(i, _)| *i == id) {
        q.remove(pos);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;
    use crate::platform::fs::{WriteError, WriteStep};
    use std::path::PathBuf;
    use std::time::Duration;

    fn empty_ref() -> ReferenceText {
        ReferenceText::default()
    }

    fn snap(id: u64, file: WorkFile, bytes: &[u8]) -> Snapshot {
        Snapshot { id: SnapshotId::new(id), file, bytes: Arc::from(bytes) }
    }

    fn write_err(class: WriteErrorClass) -> WriteError {
        WriteError { class, step: WriteStep::WriteData, os_code: None, os_text: "x".into(), path: PathBuf::from("/x") }
    }

    #[test]
    fn new_starts_without_deadline_and_default_tracks() {
        let t = PersistTracker::new(SaveInterval::S30, empty_ref(), empty_ref());
        assert_eq!(t.deadline(), None);
        assert_eq!(t.interval(), SaveInterval::S30);
        assert!(!t.playlist_dirty());
        assert!(!t.playlist_forbidden());
        assert!(t.auto_allowed(ConfigFile::Settings));
        assert!(t.auto_allowed(ConfigFile::State));
    }

    #[test]
    fn on_state_changed_user_arms_deadline_once() {
        let mut t = PersistTracker::new(SaveInterval::S10, empty_ref(), empty_ref());
        let now = ClockInstant::START;
        t.on_state_changed(Origin::User, now);
        let armed = t.deadline().expect("deadline armed");
        assert_eq!(armed, now.saturating_add(Duration::from_secs(10)));

        // Второе изменение User не переставляет уже идущий срок.
        let later = now.saturating_add(Duration::from_secs(5));
        t.on_state_changed(Origin::User, later);
        assert_eq!(t.deadline(), Some(armed));
    }

    #[test]
    fn on_state_changed_program_does_not_arm_deadline() {
        let mut t = PersistTracker::new(SaveInterval::S10, empty_ref(), empty_ref());
        t.on_state_changed(Origin::Program, ClockInstant::START);
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn on_playlist_changed_bumps_seq_and_arms_deadline() {
        let mut t = PersistTracker::new(SaveInterval::S10, empty_ref(), empty_ref());
        assert!(!t.playlist_dirty());
        let now = ClockInstant::START;
        t.on_playlist_changed(now);
        assert!(t.playlist_dirty());
        assert_eq!(t.deadline(), Some(now.saturating_add(Duration::from_secs(10))));
    }

    #[test]
    fn set_interval_shrinks_running_deadline_to_min() {
        let mut t = PersistTracker::new(SaveInterval::S120, empty_ref(), empty_ref());
        let now = ClockInstant::START;
        t.on_playlist_changed(now);
        let old_deadline = t.deadline().expect("armed");
        t.set_interval(SaveInterval::S10, now);
        let new_deadline = t.deadline().expect("still armed");
        assert!(new_deadline < old_deadline);
        assert_eq!(new_deadline, now.saturating_add(Duration::from_secs(10)));
        assert_eq!(t.interval(), SaveInterval::S10);
    }

    #[test]
    fn set_interval_without_deadline_stays_disarmed() {
        let mut t = PersistTracker::new(SaveInterval::S120, empty_ref(), empty_ref());
        t.set_interval(SaveInterval::S10, ClockInstant::START);
        assert_eq!(t.deadline(), None);
        assert_eq!(t.interval(), SaveInterval::S10);
    }

    #[test]
    fn poll_deadline_clears_and_reports_due_files_only_when_reached() {
        let mut t = PersistTracker::new(SaveInterval::S10, empty_ref(), empty_ref());
        let now = ClockInstant::START;
        t.on_playlist_changed(now);
        let before = now.saturating_add(Duration::from_secs(9));
        assert_eq!(t.poll_deadline(before), DueFiles::default());
        assert!(t.deadline().is_some());

        let at = now.saturating_add(Duration::from_secs(10));
        let due = t.poll_deadline(at);
        assert_eq!(due, DueFiles { state: true, playlist: true });
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn toml_needs_write_uses_reference_when_no_snapshot_in_flight() {
        let t = PersistTracker::new(SaveInterval::S30, ReferenceText::of(Arc::from(&b"old"[..])), empty_ref());
        assert!(!t.toml_needs_write(ConfigFile::Settings, b"old"));
        assert!(t.toml_needs_write(ConfigFile::Settings, b"new"));
        // Пустой эталон отличается от любого текста.
        assert!(t.toml_needs_write(ConfigFile::State, b""));
    }

    #[test]
    fn toml_needs_write_uses_last_sent_snapshot_when_in_flight() {
        let mut t = PersistTracker::new(SaveInterval::S30, ReferenceText::of(Arc::from(&b"old"[..])), empty_ref());
        let s = snap(0, WorkFile::Settings, b"pending");
        t.sent(&s, None);
        // Текст совпадает с уже отправленным снимком — повторная запись не нужна,
        // хотя он всё ещё отличается от эталона.
        assert!(!t.toml_needs_write(ConfigFile::Settings, b"pending"));
        assert!(t.toml_needs_write(ConfigFile::Settings, b"old"));
        assert!(t.toml_needs_write(ConfigFile::Settings, b"newer"));
    }

    #[test]
    fn playlist_dirty_and_writable_respect_seq_forbidden_and_stopped() {
        let mut t = PersistTracker::new(SaveInterval::S30, empty_ref(), empty_ref());
        assert!(t.playlist_writable(true));
        assert!(t.playlist_writable(false));

        t.on_playlist_changed(ClockInstant::START);
        assert!(t.playlist_dirty());

        let fail = WriterReply::Failed { file: WorkFile::Playlist, id: SnapshotId::new(0), err: write_err(WriteErrorClass::Io) };
        let s = snap(0, WorkFile::Playlist, b"m3u");
        t.sent(&s, Some(1));
        t.on_reply(&fail);
        // stopped блокирует только запись по отсчёту.
        assert!(!t.playlist_writable(true));
        assert!(t.playlist_writable(false));

        t.forbid_playlist();
        // forbidden блокирует запись навсегда, независимо от by_timer.
        assert!(!t.playlist_writable(true));
        assert!(!t.playlist_writable(false));
    }

    #[test]
    fn on_reply_written_updates_reference_written_seq_and_clears_stopped() {
        let mut t = PersistTracker::new(SaveInterval::S30, empty_ref(), empty_ref());

        let s = snap(0, WorkFile::Settings, b"text");
        t.sent(&s, None);
        let effect = t.on_reply(&WriterReply::Written { file: WorkFile::Settings, id: SnapshotId::new(0) });
        assert_eq!(effect, ReplyEffect::Succeeded(WorkFile::Settings));
        assert!(!t.toml_needs_write(ConfigFile::Settings, b"text"));
        assert!(t.toml_needs_write(ConfigFile::Settings, b"other"));
        assert_eq!(t.last_in_flight(WorkFile::Settings), None);

        t.on_playlist_changed(ClockInstant::START);
        let p = snap(1, WorkFile::Playlist, b"m3u");
        t.sent(&p, Some(1));
        let effect = t.on_reply(&WriterReply::Written { file: WorkFile::Playlist, id: SnapshotId::new(1) });
        assert_eq!(effect, ReplyEffect::Succeeded(WorkFile::Playlist));
        assert!(!t.playlist_dirty());
        assert!(!t.timer_stopped(WorkFile::Playlist));
    }

    #[test]
    fn on_reply_failed_sets_stopped_without_touching_reference_or_seq() {
        let mut t = PersistTracker::new(SaveInterval::S30, empty_ref(), ReferenceText::of(Arc::from(&b"ref"[..])));
        let s = snap(0, WorkFile::State, b"new text");
        t.sent(&s, None);

        let effect = t.on_reply(&WriterReply::Failed {
            file: WorkFile::State,
            id: SnapshotId::new(0),
            err: write_err(WriteErrorClass::NoSpace),
        });
        assert_eq!(effect, ReplyEffect::Failed(WorkFile::State, WriteErrorClass::NoSpace));
        assert!(t.timer_stopped(WorkFile::State));
        assert_eq!(t.last_in_flight(WorkFile::State), None);
        // Эталон не продвинулся: текст, совпадающий со старым эталоном, всё ещё не требует записи.
        assert!(!t.toml_needs_write(ConfigFile::State, b"ref"));
    }

    #[test]
    fn on_reply_superseded_drops_in_flight_without_effect() {
        let mut t = PersistTracker::new(SaveInterval::S30, empty_ref(), empty_ref());
        let s = snap(0, WorkFile::Settings, b"first");
        t.sent(&s, None);
        let s2 = snap(1, WorkFile::Settings, b"second");
        t.sent(&s2, None);

        let effect = t.on_reply(&WriterReply::Superseded { file: WorkFile::Settings, id: SnapshotId::new(0) });
        assert_eq!(effect, ReplyEffect::None);
        assert_eq!(t.last_in_flight(WorkFile::Settings), Some(SnapshotId::new(1)));
        assert!(!t.timer_stopped(WorkFile::Settings));
    }

    #[test]
    fn forbid_auto_and_forbid_playlist_block_future_writes_permanently() {
        let mut t = PersistTracker::new(SaveInterval::S30, empty_ref(), empty_ref());
        assert!(t.auto_allowed(ConfigFile::State));
        t.forbid_auto(ConfigFile::State);
        assert!(!t.auto_allowed(ConfigFile::State));
        assert!(t.auto_allowed(ConfigFile::Settings));

        assert!(!t.playlist_forbidden());
        t.forbid_playlist();
        assert!(t.playlist_forbidden());
        assert!(!t.playlist_writable(false));
    }

    /// ТЗ-12, §6.5: изменение после снимка оставляет флаг взведённым и после
    /// успешной записи этого снимка.
    #[test]
    fn playlist_flag_kept_when_changed_after_snapshot() {
        let mut t = PersistTracker::new(SaveInterval::S30, empty_ref(), empty_ref());
        t.on_playlist_changed(ClockInstant::START);
        let id = t.next_id();
        let s = Snapshot { id, file: WorkFile::Playlist, bytes: Arc::from(&b"m3u-1"[..]) };
        t.sent(&s, Some(1));
        t.on_playlist_changed(ClockInstant::START);
        assert_eq!(t.on_reply(&WriterReply::Written { file: WorkFile::Playlist, id }), ReplyEffect::Succeeded(WorkFile::Playlist));
        assert!(t.playlist_dirty());
    }

    /// §2.2: номер снимка растёт монотонно и общий для всех файлов.
    #[test]
    fn next_id_monotonic_shared() {
        let mut t = PersistTracker::new(SaveInterval::S30, empty_ref(), empty_ref());
        let a = t.next_id();
        let b = t.next_id();
        let c = t.next_id();
        assert!(a < b && b < c);
    }
}
