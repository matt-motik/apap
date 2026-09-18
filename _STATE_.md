# Текущая микро-сессия

- **Задача из ROADMAP:** A3.5 — UI вкладки Audio (capabilities, validation, фильтры, Advanced draft-only) + `sync_capabilities_and_validation`/`sync_dsd_chain_desc`
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `ui/settings.slint`
  - `ui/app.slint`
  - `src/app/ui_manager.rs`
  - `src/app/mod.rs`
  - `src/audio/player.rs` *(доп.: threading resampler-политик в OutputRequest, §7.7)*
  - `src/audio/output.rs` *(доп.: `output_device_infos()` для `audio_device_infos`, §8.1)*
- **Критерий успеха (Definition of Done):** вкладка Audio собрана по §7; превью Capabilities/Validation и фильтры работают из draft; Advanced пишет в `settings_draft` и применяется в `settings-save` через player-сеттеры; `cargo check` + `cargo clippy` (0 новых) + `cargo test` зелёные

## Итерационный трекер
[x] Шаг 1: `output.rs` — `output_device_infos()` + `device_pairs_from_infos()` (рефактор `output_devices`). Проверка: `cargo check` + unit-тесты на пары/дедуп
[x] Шаг 2: `player.rs` — поля+сеттеры `resampler_mode`/`fixed_rate`/`prefer_family`/`fallback_rate`, threading в `OutputRequest` (`open_pcm`/`open_dop`). Проверка: `cargo check` + `cargo test audio::player` без регрессий
[x] Шаг 3: `settings.slint` — structs `DeviceCapability`/`ValidationRow` + 14 properties + 10 callbacks (§7.2) + вёрстка вкладки Audio (§7.1/§7.3). Проверка: `cargo check`
[ ] Шаг 4: `app.slint` — root-свойства + forwarding блока Settings (новые props/callbacks). Проверка: `cargo check`
[ ] Шаг 5: `app/mod.rs` — `audio_device_infos: Vec<DeviceInfo>`, `FIXED_RATES`, 10 draft-колбэков (§7.7, вкл. авто-коррекцию Fixed+Fail → Nearest + status), применение в `settings-save` (сеттеры player + `apply_audio_filter` + `sync_capabilities_and_validation`). Проверка: `cargo check`
[ ] Шаг 6: `app/ui_manager.rs` — `sync_audio_devices`/`drain_audio_devices` под `Vec<DeviceInfo>` + фильтры из draft + `apply_audio_filter`, `current_audio_device_info`, `sync_capabilities_and_validation`, `build_capabilities`, `sync_dsd_chain_desc`, `sync_audio_advanced`. Проверка: `cargo check` + `cargo clippy` 0 новых
[ ] Шаг 7: bin-тесты в `app/mod.rs` (FIXED_RATES, build_capabilities, фильтры, авто-коррекция). Проверка: `cargo test` целиком зелёный

- **Текущий шаг (current_step):** Шаг 4
- **Следующий ход:** root-свойства + forwarding блока Settings в `app.slint`
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress