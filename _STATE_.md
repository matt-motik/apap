# Текущая микро-сессия

- **Задача из ROADMAP:** A2.0-3.4 — Тест-детектор аллокаций в колбэках (спека `docs/spec_audio_core_v2.0.md` §3.4)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/player.rs`
- **Критерий успеха (Definition of Done):** counting allocator (+thread-local counter) установлен в `#[cfg(test)]`, все 5 колбэков показывают 0 аллокаций в одном измерении; полный `cargo test` + `cargo clippy` зелёные.

## Итерационный трекер
[ ] Шаг 1: Добавить `#[cfg(test)] mod alloc_tracking` (обёртка над `System`, счётчик в thread-local + `TRACKING`-флаг) и `#[global_allocator]`. Проверка: `cargo check` ок.
[ ] Шаг 2: Юнит-тест `all_callbacks_perform_zero_allocations`: warm-up всех 5 колбэков, затем 5 измерений (delta == 0 на каждом). Проверка: `cargo test all_callbacks_perform_zero_allocations` зелёный.
[ ] Шаг 3: Полный `cargo test` (lib+bin) + `cargo clippy`; 0 новых предупреждений в player.rs. Проверка: полный прогон ок.

- **Текущий шаг (current_step):** Шаг 1
- **Следующий ход:** Вставить counting-аллокатор в `src/audio/player.rs`.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress