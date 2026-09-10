# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммится вместе с изменениями кода.

## Статус

- **Состояние:** `done` — рефакторинг системы колонок плейлиста завершён и подтверждён пользователем вживую.

## Активная задача

Нет. Текущая задача (конфиг-driven `[columns]` + now-playing + dbl-click + scroll-to-playing + кнопка repeat) выполнена и проверена. Следующая задача — по решению пользователя (ROADMAP.md / инбокс `_TODO_/`).

## Выполнено

### ColumnCfg и конфиг-driven колонки
- `ColumnCfg` struct в `src/settings.rs`: title, priority, min_width, max_width, max_width_percent, visible, column_type, width
- `default_columns()` — 14 колонок с русскими заголовками и tuned limits
- `Settings`: `columns: HashMap<String, ColumnCfg>`, `scroll_to_playing: bool`, `column_order: Vec<String>`
- Методы: `column_cfg()`, `column_title()`, `column_visible()`, `column_width_pct()`, `normalize_visible_pct()`, `enable_column()`, `disable_column()`, `ordered_columns()`, `move_column()`
- `LegacySettings` + `migrate_legacy_columns()` для миграции старого TOML

### NowPlaying колонка
- `ColumnId::Index` → `ColumnId::NowPlaying` (ключ `"now_playing"`)
- `build_row()`: показывает `▶` для текущего трека
- `apply_sort()` и `sort_rows_text/compare` — блокируют/обрабатывают `NowPlaying`
- Bitrate отображается без «kbps»

### Play с первого трека
- `Player::has_decoder()` в `src/audio/player.rs`
- `on_play_pause`: если декодера нет — `play_track(current.unwrap_or(0))` (первый трек плейлиста)

### Double-click
- Slint: `row-pointer-event` через `PointerEventKind.up` (нет `dbl-click` в enum)
- Rust: `last_click_row` + `last_click_time` + 400ms threshold — двойной клик = play, одиночный = выделение

### Scroll-to-playing
- `ui/playlist.slint`: `public function do-scroll-to-row(index)` вызывает `view.set-current-row(index)`
- `ui/app.slint`: `callback scroll-to-row(int)` → `playlist_view_width.do-scroll-to-row(idx)`
- `playback_manager.rs`: `play_track()` вызывает `set_current_row` + `invoke_scroll_to_row` (если `scroll_to_playing: true`)
- Настройка `scroll_to_playing: bool` (default true) в `settings.rs`

### Кнопка repeat (Off→All→One)
- `ui/icons/repeat-one.svg` — новая монохромная иконка 🔂
- `cycle_repeat()` в `playback_manager.rs`; одна кнопка в top_panel.slint (repeat/repeat-one)
- Shuffle/repeat синхронизируются из конфига при старте (`sync_settings_to_ui`)

### Верификация (вживую пользователем)
- Play после старта: играет первый отображаемый трек, маркер `▶` ставится корректно
- Режимы shuffle и repeat — корректны
- `cargo check` — OK (warning только padding)
- `cargo test` — 65 тестов (59 lib + 6 bin) зелёные
- `cargo clippy` — без новых warning

## Изменяемые файлы (коммит `CONFIG-driven-columns`)

- `src/settings.rs` — ColumnCfg, Settings, migration
- `src/playlist_layout.rs` — resolve_widths с limits параметром
- `src/app/ui_manager.rs` — build_row (▶), build_table_columns, save_column_widths, dialog_cols_model
- `src/app/playback_manager.rs` — play_track (set_current_row + invoke_scroll_to_row), cycle_repeat
- `src/app/playlist_manager.rs` — apply_sort (NowPlaying guard)
- `src/app/mod.rs` — on_play_pause (has_decoder), double-click detection, reset_columns
- `src/audio/player.rs` — has_decoder()
- `src/playlist.rs` — sort_rows_text/compare (NowPlaying)
- `ui/playlist.slint` — do-scroll-to-row function
- `ui/app.slint` — scroll-to-row callback
- `ui/top_panel.slint` — кнопка repeat (repeat/repeat-one)
- `ui/icons/repeat-one.svg` — новая иконка
- `ROADMAP.md` — пункт 4.12
- `_STATE_.md`