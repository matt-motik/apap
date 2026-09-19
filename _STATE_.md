# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-B6 (устройство не возвращается в микшер на паузе/трей/выходе) + V5.1-B7 (размер/положение окна не восстанавливаются и не сохраняются)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/player.rs`
  - `src/app/mod.rs`
  - `src/app/ui_manager.rs`
  - `src/main.rs`
- **Критерий успеха (Definition of Done):**
  - B6: пауза освобождает exclusive-движок; ресюм переоткрывает поток и восстанавливает позицию; hide-в-трей и оба quit-пути вызывают release. `cargo test --lib player::pause_releases_exclusive_and_resume_restores_position` зелёный.
  - B7: `apply_window_geometry` повторно вызывается после `ui.show()`; `tick` сохраняет геометрию с debounce ~2 c. Все тесты и clippy без новых предупреждений.

## Итерационный трекер
[x] Шаг 1 (B6): `player.rs` — пауза в `toggle()` освобождает exclusive-движок; `play()`/resume используют `reopen_and_seek(resume_pos_secs)` для переоткрытия с сохранением позиции. Проверка: `cargo check`.
[x] Шаг 2 (B6): `mod.rs` — `TrayCmd::ShowHide` при hide вызывает `player.release_if_exclusive()`; `close_requested` (реальный quit) и `TrayCmd::Quit` вызывают `player.release_engine()`. Проверка: `cargo check`.
[x] Шаг 3 (B7): `mod.rs` + `ui_manager.rs` — поля `win_geom_dirty`/`win_geom_changed`, метод `track_window_geometry` с debounce 2 c, вызов из `tick`. Проверка: `cargo check`.
[x] Шаг 4 (B7): `main.rs` — повторный вызов `apply_window_geometry()` после `ui.show()`; visibility `pub(crate)`. Проверка: `cargo check`.
[x] Шаг 5: Тест `pause_releases_exclusive_and_resume_restores_position` (exclusive-hw); полный прогон `cargo test` + `cargo clippy`. Проверка: зелёные.
[ ] Шаг 6: Коммит B6+B7 и закрытие сессии (ROADMAP, _STATE_.md done).

- **Текущий шаг (current_step):** Шаг 6 — коммит и закрытие
- **Следующий ход:** git-коммит изменённых файлов и документации
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress