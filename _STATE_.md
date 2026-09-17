# Текущая микро-сессия

- **Задача из ROADMAP:** A2.0-3.1 — Преаллокация `scratch` до `MAX_OUT_SAMPLES` в `open_pcm`/`open_dop` (спека `docs/spec_audio_core_v2.0.md` §3.1)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/player.rs`
- **Критерий успеха (Definition of Done):** `cargo test audio::player` зелёный; после первого колбэка `scratch.capacity() == MAX_OUT_SAMPLES`; `cargo clippy` без новых предупреждений.

## Итерационный трекер
[ ] Шаг 1: Добавить const `MAX_OUT_SAMPLES = 1 << 16` и выделять `scratch = vec![0.0f32; MAX_OUT_SAMPLES]` в `open_pcm`/`open_dop` (заменить текущий `Vec::with_capacity(8192)` + `clear()`). Проверка: `cargo check` ок; тест `scratch_capacity_is_pinned` зелёный.
[ ] Шаг 2: Написать юнит-тест `scratch_capacity_is_pinned`: после симуляции open (seed_core) проверять, что scratch имеет capacity ≥ MAX_OUT_SAMPLES и не растёт/не сжимается после колбэка. Проверка: `cargo test scratch_capacity_is_pinned` зелёный.
[ ] Шаг 3: Прогнать `cargo test` (полный) + `cargo clippy`; зафиксировать 0 новых предупреждений. Проверка: полный тест-ран и clippy ок.

- **Текущий шаг (current_step):** Шаг 1
- **Следующий ход:** Внести правку в `src/audio/player.rs`: константа `MAX_OUT_SAMPLES`, инициализация `scratch` в `open_pcm` и `open_dop`.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress