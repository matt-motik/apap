# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-8.6 — DoP: UI-переключатель DSD-режима (уже сделан в V5.1-7.4) + чекбокс «Bit-perfect» в настройках звука (§7.5/§8.6); отладка DoP-бит-потока на реальном ЦАП — ручная (железо не доступно)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `ui/settings.slint`
  - `ui/app.slint`
  - `src/app/ui_manager.rs`
  - `src/app/mod.rs`
- **Критерий успеха (Definition of Done):** в настройках (вкладка Audio) появляется чекбокс «Bit-perfect», синхронизируется из live-настроек при открытии диалога; при переключении в диалоге пишется draft (live-warn DSD актуализируется); на Save изменение применяется к плееру (`player.set_bit_perfect` + unity gain/размут при активации) и персистится; `cargo check`, `cargo test`, `cargo clippy` зелёные без новых предупреждений.

## Итерационный трекер

[x] Шаг 1: ui/settings.slint — проп `bit-perfect` + callback `set-bit-perfect(bool)` + CheckBox «Bit-perfect» в Audio-вкладке (V5.1-8.6.1). Проверка: `cargo check` без ошибок.

[x] Шаг 2: ui/app.slint — root-проп `settings-bit-perfect`, callback `settings-set-bit-perfect(bool)`, binding + mapping в инстансе Settings. Проверка: `cargo check` без ошибок.

[ ] Шаг 3: src/app/ui_manager.rs + src/app/mod.rs — `sync_settings_to_ui` → `set_settings_bit_perfect`; callback `settings-set-bit-perfect` (draft + live-warn); в `on_settings_save` diff-применение bit_perfect (player + unity gain) при изменении. Проверка: `cargo check` + `cargo test` зелёные.

[ ] Шаг 4: Верификация (`cargo test`, `cargo clippy` 0 новых), ROADMAP V5.1-8.6 → ✅ (+ подзадачи, пометка про ручную DoP-проверку на железе), `_STATE_.md` → done, коммит. Проверка: тесты зелёные, clippy чист.

- **Текущий шаг (current_step):** Шаг 3
- **Следующий ход:** Sync set_settings_bit_perfect в ui_manager + callback settings-set-bit-perfect + diff-применение в on_settings_save в mod.rs
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress