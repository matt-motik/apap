# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-B4 — не сохраняется состояние окна fullscreen/maximized
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/settings.rs
  - src/app/ui_manager.rs
  - ROADMAP.md
  - _STATE_.md
- **Критерий успеха (Definition of Done):** `cargo test` (новый roundtrip-тест флагов win_fullscreen/win_maximized) + `cargo clippy` 0 новых + `cargo check` зелёные

## Итерационный трекер
[x] Шаг 1: Поля `win_fullscreen`/`win_maximized` в `Settings` (+ `serde`/`Default`) + roundtrip-тест в settings.rs. Проверка: `cargo test` — тест зелёный
[ ] Шаг 2: `save_window_geometry` — запись флагов `is_fullscreen()`/`is_maximized()`; размер/позицию сохранять только из обычного состояния. Проверка: `cargo check`
[ ] Шаг 3: `apply_window_geometry` — восстановление `set_maximized`/`set_fullscreen` после размера/позиции. Проверка: `cargo check` + `cargo clippy` + `cargo test` целиком зелёные

- **Текущий шаг (current_step):** Шаг 2
- **Следующий ход:** Обновить save_window_geometry в src/app/ui_manager.rs
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress