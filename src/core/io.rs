//! Поток `apap-io` (ADR-20): чтение тем, подсчёт и очистка дисковых кэшей
//! вне UI-потока; поколение на вид задания, ответы разбираются на тике
//! 100 мс (§2.12, ТЗ-22, ТЗ-34).
//!
//! Этот шаг даёт только типы задания/ответа и трейт `IoWorker`; сам поток
//! (`spawn_io`) — следующий микро-шаг.

use std::sync::Arc;

use crate::persist::settings_file::ThemeName;
use crate::theme::{ThemeData, ThemeEntry};

/// Задание потоку `apap-io` (§2.12, ADR-20, ТЗ-22, ТЗ-34).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IoJob {
    /// Чтение файла темы при открытии диалога и при выборе темы (ТЗ-22).
    ReadTheme { gen: u64, name: ThemeName },
    /// ОТКЛОНЕНИЕ от §2.12: список тем из `themes/` нужен диалогу настроек
    /// без обхода каталога в UI-потоке (ТЗ-22); в спецификации задания нет.
    ListThemes { gen: u64 },
    /// Подсчёт размера дисковых кэшей при открытии диалога и после очистки
    /// (ТЗ-22).
    CacheSizes { gen: u64 },
    /// Очистка дисковых кэшей вне UI-потока (ТЗ-34).
    ClearCache { gen: u64, which: CacheKind },
}

impl IoJob {
    /// Поколение задания — для сопоставления с ответом на тике (ADR-20).
    pub fn gen(&self) -> u64 {
        match self {
            IoJob::ReadTheme { gen, .. }
            | IoJob::ListThemes { gen }
            | IoJob::CacheSizes { gen }
            | IoJob::ClearCache { gen, .. } => *gen,
        }
    }
}

/// Ответ потока `apap-io`, разбираемый на тике 100 мс (§2.12, ADR-20).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IoDone {
    /// Результат `IoJob::ReadTheme` (ТЗ-22, ТЗ-27, ТЗ-29).
    Theme { gen: u64, result: Result<Arc<ThemeData>, Box<str>> },
    /// Результат `IoJob::ListThemes` — ОТКЛОНЕНИЕ от §2.12, см. `IoJob`.
    Themes { gen: u64, entries: Vec<ThemeEntry> },
    /// Результат `IoJob::CacheSizes` (ТЗ-22).
    CacheSizes { gen: u64, sizes: CacheSizes },
    /// Результат `IoJob::ClearCache` — новый размер после очистки (ТЗ-34).
    Cleared { gen: u64, sizes: CacheSizes },
}

impl IoDone {
    /// Поколение задания, которому отвечает этот результат (ADR-20).
    pub fn gen(&self) -> u64 {
        match self {
            IoDone::Theme { gen, .. }
            | IoDone::Themes { gen, .. }
            | IoDone::CacheSizes { gen, .. }
            | IoDone::Cleared { gen, .. } => *gen,
        }
    }
}

/// Какие дисковые кэши очищать (`IoJob::ClearCache`, ТЗ-34).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheKind {
    Visualization,
    Covers,
    All,
}

/// Размер дисковых кэшей в байтах (ТЗ-22). RAM-кэши считаются в UI-потоке,
/// сюда не входят.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheSizes {
    pub viz_disk: u64,
    pub covers: u64,
}

/// Канал заданий/ответов потока `apap-io` (§2.12, ADR-20).
pub trait IoWorker {
    /// Поставить задание в очередь потока `apap-io`.
    fn submit(&self, job: IoJob);
    /// Забрать готовый ответ без блокировки; `None` — ответа пока нет.
    fn try_recv(&self) -> Option<IoDone>;
}
