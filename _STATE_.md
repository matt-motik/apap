# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-8.5 — Валидация `target_sample_rate = auto` → ближайшая поддерживаемая частота ЦАП, [ТЗ §8.5](docs/spec_visualizer_v5.1.md#85-валидация)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/output.rs`
- **Критерий успеха (Definition of Done):** чистая функция `nearest_rate(ranges, channels, requested)` возвращает ближайший поддерживаемый ЦАП-рейт (точное попадание → `requested`, иначе ближайшая граница диапазона, при равной дистанции — меньшая частота); интегрирована в `choose_output` (fallback вместо `device.sample_rate`, когда точный рейт не поддерживается); unit-тесты покрывают exact/fallback/семейства 44.1/48k/tie-break/отсутствие канала; `cargo check`, `cargo test audio::output`, `cargo clippy` зелёные без новых предупреждений.

## Итерационный трекер

[x] Шаг 1: output.rs — чистая функция `nearest_rate(ranges, channels, requested) -> Option<u32>` (V5.1-8.5.1). Проверка: `cargo check` без ошибок.

[ ] Шаг 2: output.rs — unit-тесты `nearest_rate_*`: exact попадание, fallback по границе, семейства 44.1/48k, tie-break к меньшей, miss по каналам, пустой список (V5.1-8.5.2). Проверка: `cargo test output::nearest_rate` зелёный.

[ ] Шаг 3: output.rs — интеграция в `choose_output`: когда `track_rate` не поддерживается — ближайший рейт через `nearest_rate` (V5.1-8.5.3) + тест `choose_output_picks_nearest_supported_when_unsupported`. Проверка: `cargo test audio::output` + `cargo check` зелёные.

[ ] Шаг 4: Полная верификация (`cargo test`, `cargo clippy` 0 новых), ROADMAP V5.1-8.5 + подзадачи → ✅ (+ «активный следующий ход»), `_STATE_.md` → done, коммит. Проверка: тесты зелёные, clippy чист.

- **Текущий шаг (current_step):** Шаг 2
- **Следующий ход:** Написать unit-тесты nearest_rate_* в src/audio/output.rs
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress