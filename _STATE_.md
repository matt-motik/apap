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
[x] Шаг 2: `save_window_geometry` — запись флагов `is_fullscreen()`/`is_maximized()`; размер/позицию сохранять только из обычного состояния. Проверка: `cargo check`
[x] Шаг 3: `apply_window_geometry` — восстановление `set_maximized`/`set_fullscreen` после размера/позиции. Проверка: `cargo check` + `cargo clippy` + `cargo test` целиком зелёные

- **Текущий шаг (current_step):** Шаг 3 завершён — переход к Шагу 5 (Завершение сессии)
- **Следующий ход:** Отметить V5.1-B4 в ROADMAP как done, законсервировать _STATE_.md, финальный аудит
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress