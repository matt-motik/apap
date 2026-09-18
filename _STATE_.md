# Текущая микро-сессия

- **Задача из ROADMAP:** A3.4 — Player: DSD preference chain, exclusive retry, StreamDesc, try_open_dsd, TestHooks
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/audio/output.rs
  - src/audio/player.rs
- **Критерий успеха (Definition of Done):** §11.4 тесты + существующие зелёные; `cargo check`/`cargo clippy` без новых варнингов; `cargo test` 211 lib + 13 bin зелёные

## Итерационный трекер
[x] Шаг 1: output.rs — расширить `OutputSpec` (exclusive/resampled/source_rate/source_channels/fallback), добавить `select_output_for(&OutputRequest)`, делегировать через него `select_output` (degraded-дефолт). Проверка: `cargo check` без ошибок.

[x] Шаг 2: player.rs — `StreamDesc` + `stream_desc()` + поля политик `exclusive_mode`/`fallback_policy` + setters + `format_name(&'static str)`. Проверка: `cargo check` без ошибок.

[x] Шаг 3: player.rs — refactor `open`: `is_dsd ? open_dsd_with_chain : open_pcm`; `try_open_dsd` (Native→Err, DoP→open_dop, Pcm→open_pcm); запись геометрии в `stream_desc` в `open_pcm`/`open_dop`; DSD-поля (dsd_mode/dsd_preferred/dsd_fallback_reason) — в `open_dsd_with_chain`. Проверка: `cargo test audio::player` зелёный.

[x] Шаг 4: player.rs — exclusive retry в `start_engine` (Auto→1 retry shared, Strict→Err), `stream_desc.exclusive_fallback` при запрошенном, но не выданном exclusive. Проверка: `cargo test audio::player` зелёный.

[x] Шаг 5: player.rs — `TestHooks { build_failures }` (cfg test) в `start_engine` + тесты §11.4 (env-gated: MUSIC_PCM_TEST_FILE/MUSIC_DSD_TEST_FILE/MUSIC_DFF_TEST_FILE). Проверка: `cargo test audio::player` + `cargo clippy` без новых.

- **Текущий шаг (current_step):** Шаг 5 (завершён)
- **Следующий ход:** Финальная верификация и закрытие сессии (ROADMAP A3.4 → ✅, консервация _STATE_)
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress