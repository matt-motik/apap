//! Модель плейлиста: идентификаторы строк и ключ сортировки (ADR-15, §3.1, ТЗ-42, ТЗ-43).

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
