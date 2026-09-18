# Текущая микро-сессия

- **Задача из ROADMAP:** A3.3 — Backend: `choose_output` по политикам, `FallbackReason`, `ChosenOutput`, `describe_stream`; `validate_audio_settings` (11 строк) + тесты
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/output.rs`
- **Критерий успеха (Definition of Done):** `cargo check` без ошибок, `cargo clippy` без новых предупреждений, `cargo test audio::output` зелёный (тесты §11.2 choose_output_* + describe_stream; §11.3 validate_*)

## Итерационный трекер

[x] Шаг 1: Типы данных: `FallbackReason`, `OutputRequest` (+`from_settings`/`with_source`), расширение `ChosenOutput` полями (exclusive/resampled/source_rate/source_channels/fallback). Проверка: `cargo check` без ошибок.

[x] Шаг 2: Переписать `choose_output` на `&OutputRequest` по алгоритму §3.4 (exclusive → каналы → rate mode Native/Fixed/Auto + fallback-политики → sample format → buffer) + helpers (`clock_family_of`, `same_family`, `resolve_family`, `nearest_in_family`, `nearest_rate_at_least`); адаптировать `select_output`/`probe_output`. Проверка: `cargo check` без ошибок.

[x] Шаг 3: Обновить существующие тесты `output.rs` на новый API (`req_defaults`/`select_from_host`) — сохранить старое поведение. Проверка: `cargo test audio::output` зелёный (46).

[ ] Шаг 4: `describe_stream` + `Outcome`/`ValidationRow`/`validate_audio_settings` (8 PCM + 3 DSD строки, DSD-цепочка по `settings.dsd.mode`). Проверка: `cargo check` без ошибок.

[ ] Шаг 5: Тесты §11.2: exclusive strict/auto на сервере, Fixed (fixed_rate/family/unsupported→nearest/empty family→default), fallback-политики (SameFamily/NeverDownsample/Fail), Native, DeviceDefault, `describe_stream_includes_resampled_marker`. Тесты §11.3: `validate_*` (native/nearest/unsupported/cross-family/bp/native/DSD). Проверка: `cargo test audio::output` + `cargo clippy` без новых.

- **Текущий шаг (current_step):** Шаг 4
- **Следующий ход:** `describe_stream` + `Outcome`/`ValidationRow`/`validate_audio_settings` (11 строк)
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress