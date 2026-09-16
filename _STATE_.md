# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-B3 — ADI-2: выбирать raw `hw:`-узел среди дескрипторов одного имени; конфиг хранит стабильный pcm_id (id), GUI — человекочитаемое имя; разделить DoP/i32-PCM колбэки
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/audio/output.rs
  - src/audio/player.rs
  - src/app/ui_manager.rs
  - src/app/mod.rs
- **Критерий успеха (Definition of Done):** cargo test (все зелёные), cargo clippy без новых предупреждений; выбор ADI-2 резолвит марта raw `hw:CARD=…` узел (I32, точные рейты), DoP-колбэк применяется только при `is_dop`, обычный PCM на I32 — через `audio_callback_i32_pcm`

## Итерационный трекер
[x] Шаг 1: `DeviceInfo.id` + `collapse_same_name` (дедуп по имени, приоритет raw `hw:`) в `CpalHost::devices()`; удалить временный `diag_adi_select`; юнит-тест collapse. Проверка: cargo test audio::output + cargo check
[x] Шаг 2: резолв по id-or-name в `choose_output` (поля `ChosenOutput.device_id`, `DeviceInfo.id`), `CpalHost::device_by_id` (фолбэк на имя), `select_output`/`OutputSpec.device_id`. Проверка: cargo test audio::output
[x] Шаг 3: флаг `OutputSpec.is_dop`; `open_dop` ставит его; в `build_stream` I32-арм диспетчеризует `audio_callback_i32_dop` (DoP, `<<8`) и новый `audio_callback_i32_pcm` (масштаб i32, vol/dither); тест на `audio_callback_i32_pcm`. Проверка: cargo test audio::player
[x] Шаг 4: `output_devices()` → пары `(id, label)` через `SERVER_NODE_SUFFIX`-константу и дедуп по имени; `default_device_id()`; `probe_output` резолвит id-or-name. Проверка: cargo test audio::output
[x] Шаг 5: `drain_audio_devices`/`resolve_device_label`/`find_device_index`/`device_display_name` в ui_manager.rs (+комментарии mod.rs/playback_manager.rs): пары `(id,label)`, матчинг want по id или имени(label с убранным суффиксом); label→id в `settings_device`; active-имя человекочитаемое. Проверка: cargo test bin + clippy без новых warnings (22 lib / 11 bin — только пре-существующие)

- **Текущий шаг (current_step):** Завершение (Шаг 5)
- **Следующий ход:** Обновить ROADMAP.md (статус V5.1-B3 → ✅ сделано + hash), закоммитить код+_STATE_.md, затем консервация _STATE_.md (done) + ROADMAP-коммит
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress