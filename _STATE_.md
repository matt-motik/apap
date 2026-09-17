# Текущая микро-сессия

- **Задача из ROADMAP:** A2.0-3.3 — Запрет сжатия пула в `scratch_release` (спека `docs/spec_audio_core_v2.0.md` §3.3)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/player.rs`
- **Критерий успеха (Definition of Done):** `shrink_to(64K)` удалён из `scratch_release`, capacity константна через циклы колбэков; `cargo test` + `cargo clippy` (player.rs) зелёные.

## Итерационный трекер
[ ] Шаг 1: Удалить из `scratch_release` блок `if buf.capacity() > 128 * 1024 { shrink_to }`, оставить `clear()`. Проверка: `cargo check` ок.
[ ] Шаг 2: Юнит-тест `scratch_pool_never_shrinks_across_calls` (емкость держится через 3 цикла i16-колбэка на больших буферах). Проверка: `cargo test scratch_pool_never_shrinks_across_calls` зелёный.
[ ] Шаг 3: Полный `cargo test` + `cargo clippy`; 0 новых предупреждений. Проверка: полный прогон ок.

- **Текущий шаг (current_step):** Шаг 1
- **Следующий ход:** Убрать `shrink_to` из `scratch_release` в `src/audio/player.rs`.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress