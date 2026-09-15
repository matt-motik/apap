# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-10.5.3 «Фикс: рекурсивная очистка кэша обложек + cover_cache_size() + UI-разбивка RAM/viz/cover размеров»
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/cover.rs`
  - `ui/settings.slint`
  - `ui/app.slint`
  - `src/app/ui_manager.rs`
- **Критерий успеха (Definition of Done):** `cargo check` чистый, `cargo test` зелёный, `cargo clippy` без новых предупреждений; очистка кэша обложек удаляет файлы из hash-поддиректорий; вкладка Cache показывает 3 строки (RAM viz / Viz disk / Cover disk).

## Итерационный трекер
[x] Шаг 1: `cover.rs`: рекурсивная `clear_cover_cache_in` (обход поддиректорий + удаление пустых хэш-каталогов) + обновление теста на боевой layout (файлы в поддиректориях). Проверка: `cargo test clear_cover_cache`
[ ] Шаг 2: `cover.rs`: `cover_cache_size() -> u64` + `cover_cache_size_in()` (рекурсивная сумма) + юнит-тест (глубина 2, missing dir → 0). Проверка: `cargo test cover_cache_size`
[ ] Шаг 3: `settings.slint`: 3 property (`cache-ram-size`, `cache-viz-size`, `cache-cover-size`), замена строки Disk cache на Viz cache + Cover cache, переименование RAM cache → RAM viz cache. Проверка: `cargo build`
[ ] Шаг 4: `app.slint`: `settings-cache-viz-size`/`settings-cache-cover-size` вместо `settings-cache-disk-size` + forward в Settings. Проверка: `cargo build`
[ ] Шаг 5: `ui_manager.rs`: `sync_cache_stats_to_ui` — ram + viz_disk + cover_disk (`cover::cover_cache_size()`). Проверка: `cargo check` + `cargo test` + `cargo clippy`

- **Текущий шаг (current_step):** Шаг 2
- **Следующий ход:** Добавить `cover_cache_size()` + `cover_cache_size_in()` в `src/cover.rs` + юнит-тесты.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress