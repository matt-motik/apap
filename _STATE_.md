# Текущая микро-сессия

- **Задача из ROADMAP:** A2.0-4.2 — UI-бейдж «Resample (device limit)» (спека `docs/spec_audio_core_v2.0.md` §4.2)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/app/playback_manager.rs`
  - `src/app/ui_manager.rs`
  - `src/app/mod.rs`
  - `ui/status.slint`
  - `ui/app.slint`
  - `ui/theme.slint`
- **Критерий успеха (Definition of Done):** AppWindow получает свойство `status-bp-resample`; StatusBar рендерит бейдж при `bp-resample`; Runtime-синк в тике пробрасывает `Player::bit_perfect_resampled()`; полный прогон зелёный.

## Итерационный трекер
[ ] Шаг 1: Новый цвет `text-warn` в `theme.slint`; в `status.slint` добавить `in property <bool> bp-resample` + бейдж «Resample (device limit)». Проверка: сборка `.slint` без ошибок (cargo check).
[ ] Шаг 2: В `app.slint` свойство `status-bp-resample` (in-property) и проброс `bp-resample: root.status-bp-resample`. Проверка: cargo check.
[ ] Шаг 3: В `mod.rs` (UiState) + `playback_manager.rs` delta-синк: читать `player.bit_perfect_resampled()`, при изменении `set_status_bp_resample`. Проверка: `cargo test app::` + clippy чисто.
[ ] Шаг 4: Полный `cargo test` + `cargo clippy`; 0 новых предупреждений. Проверка: полный прогон ок.

- **Текущий шаг (current_step):** Шаг 1
- **Следующий ход:** Добавить `text-warn` в theme.slint и бейдж в status.slint.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress