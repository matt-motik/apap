# Текущая микро-сессия

- **Задача из ROADMAP:** A2.0-3.2 — Guard «не растить» во всех `audio_callback_*` (спека `docs/spec_audio_core_v2.0.md` §3.2)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/player.rs`
- **Критерий успеха (Definition of Done):** все 4 колбэка (i16/u8/i32_pcm/i32_dop) отдают тишину при `data.len() > MAX_OUT_SAMPLES`, `resize` внутри потолка не аллоцирует; `cargo test` + `cargo clippy` (player.rs) зелёные.

## Итерационный трекер
[ ] Шаг 1: Добавить хелпер `PlaybackCore::scratch_for(len) -> Option<Vec<f32>>` — ceiling-check `len > MAX_OUT_SAMPLES` ДО `resize`. Проверка: `cargo check` ок.
[ ] Шаг 2: Применить `let Some(mut tmp) = c.scratch_for(data.len()) else { тишина; return }` в `audio_callback_i16`/`u8`/`i32_pcm`/`i32_dop` (заменить `scratch_f32`+`resize`). Проверка: `cargo check` ок; clippy по `player.rs` без предупреждений.
[ ] Шаг 3: Юнит-тест `callback_silences_when_buffer_exceeds_ceiling` (буфер > MAX_OUT_SAMPLES → тишина, пул не растёт). Проверка: `cargo test callback_silences_when_buffer_exceeds_ceiling` зелёный.
[ ] Шаг 4: Полный `cargo test` + `cargo clippy`; 0 новых предупреждений. Проверка: полный прогон ок.

- **Текущий шаг (current_step):** Шаг 1
- **Следующий ход:** Внести в `src/audio/player.rs` хелпер `scratch_for`, проверить сборку.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress