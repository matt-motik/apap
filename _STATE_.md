# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-B1 — Паника в DoP-фреймере при воспроизведении DSD (dop.rs:37 assertion failed: out.len() >= words * ch)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/audio/dsd.rs
  - src/audio/dop.rs
- **Критерий успеха (Definition of Done):** DoP-декодер не паникует, cargo test dsd зелёный, cargo clippy без новых предупреждений

## Итерационный трекер
[x] Шаг 1: Исправить аллокацию dop_words (with_capacity → len = raw_len) в decode_group_dop. Проверка: cargo check без ошибок, тест dsf_dop_raw_headless без паники

- **Текущий шаг (current_step):** Шаг 1
- **Следующий ход:** Коммит фикса
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress