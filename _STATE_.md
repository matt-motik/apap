# Текущая микро-сессия

- **Задача из ROADMAP:** A2.0-5.2 — `ring_buffer_ms` в `Settings` + размер ring
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/settings.rs
  - src/audio/player.rs
  - src/app/mod.rs
- **Критерий успеха (Definition of Done):** `ring_buffer_ms` (дефолт 1500, диапазон 100..10000) сохраняется в TOML-roundtrip, влияет на ёмкость ring при следующем `open`; unit-тесты ms→samples и валидации зелёные; `cargo test` + `cargo clippy` без новых предупреждений.

## Итерационный трекер
[x] Шаг 1: `src/settings.rs` — поле `ring_buffer_ms: u32` в `AudioCfg` (serde-default), константы min/max/default и `pub fn clamp_ring_buffer_ms`, тесты дефолта/roundtrip/clamp. Проверка: `cargo test settings` зелёный.
[ ] Шаг 2: `src/audio/player.rs` — поле `ring_buffer_ms`, `set_ring_buffer_ms`, `ring_capacity(ms, buffer_frames)` (нижняя граница ≥ 2× cpal-буфера при Fixed, иначе 4096), применение в `start_engine`; юнит-тест ms→samples. Проверка: `cargo test audio::player` зелёный.
[ ] Шаг 3: `src/app/mod.rs` — проброс `settings.audio.ring_buffer_ms` в `player.set_ring_buffer_ms` при инициализации. Проверка: `cargo check` + полный `cargo test` + `cargo clippy` без новых предупреждений.

- **Текущий шаг (current_step):** Шаг 2
- **Следующий ход:** Добавить `ring_buffer_ms` в `Player` и использовать в `ring_capacity`/`start_engine`.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress
