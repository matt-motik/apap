# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-10.5.2 «Кнопки очистки: «Очистить кэш визуализации», «Очистить кэш обложек», «Очистить всё»»
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/fulltrack.rs`
  - `src/cover.rs`
  - `ui/settings.slint`
  - `ui/app.slint`
  - `src/app/mod.rs`
- **Критерий успеха (Definition of Done):** `cargo check` чистый, `cargo test` зелёный, `cargo clippy` без новых предупреждений в изменённых файлах; 3 кнопки в вкладке «Cache» удаляют файлы кэша и корректно обновляют статистику RAM/Disk.

## Итерационный трекер
[x] Шаг 1: `fulltrack.rs`: `clear_disk_cache_in(dir) -> io::Result<usize>` + `clear_disk_cache()` + юнит-тесты (temp dir, missing dir). Проверка: `cargo test clear_disk_cache` зелёный (`9c87a51`)
[x] Шаг 2: `cover.rs`: `clear_cover_cache_in(dir) -> usize` + `clear_cover_cache()` + юнит-тесты. Проверка: `cargo test clear_cover_cache` зелёный (`5ca284f`)
[x] Шаг 3: `settings.slint`: 3 кнопки («Clear viz cache», «Clear cover cache», «Clear all») + 3 callback declarations в Cache tab. Проверка: `cargo build` компилирует .slint без ошибок
[ ] Шаг 4: `app.slint`: 3 forwarded callbacks (`settings-clear-viz-cache` и т.д.) через AppWindow → Settings. Проверка: `cargo build`
[ ] Шаг 5: `app/mod.rs`: привязка 3 колбэков (`on_settings_clear_viz_cache` и т.д.) — RAM clear + disk clear + sync stats. Проверка: `cargo check` + `cargo test` + `cargo clippy`

- **Текущий шаг (current_step):** Шаг 4
- **Следующий ход:** Реализовать forward callbacks (`settings-clear-viz-cache`, `settings-clear-cover-cache`, `settings-clear-all-cache`) в `ui/app.slint`: declare + привязка в инстанцировании Settings.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress