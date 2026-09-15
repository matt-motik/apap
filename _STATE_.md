# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-8.7 (дизайн-delta Bit-perfect UI, §7.5/§8.6)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `ui/top_panel.slint`
  - `ui/app.slint`
  - `src/tray.rs`
  - `src/app/mod.rs`
  - `ROADMAP.md`
  - `_STATE_.md`
- **Критерий успеха (Definition of Done):** `cargo check` + `cargo clippy` (0 новых варнингов) + `cargo test` зелёные; BP переключается только в настройках; при включении BP в трее появляется временный тултип «громкость не регулируется программно»; мёртвый/закомментированный код удалён.

## Итерационный трекер
[x] Шаг 0: Регистрация V5.1-8.7 в ROADMAP.md (таблица 6.7, снятие V5.1-7.2)
[x] Шаг 1: Slint-чистка — `ui/top_panel.slint` + `ui/app.slint`: удалить закомментированные блоки (BP CheckBox :521-525, подпись «Управляется внешним предусилителем / ОС» :498-506), unused `callback toggle-bit-perfect()` (:144) и binding на TopPanel (app.slint :428), `CheckBox` из импорта (:2), выровнять отступ placeholder-`Text`. Проверка: `cargo check` зелёный
[x] Шаг 2: Удаление toggle-хендлера — `ui/app.slint` (корневой `callback toggle-bit-perfect()` :233) + `src/app/mod.rs` (`ui.on_toggle_bit_perfect` :540-561). Проверка: `cargo check` зелёный
[ ] Шаг 3: Tray-notice — `src/tray.rs` + `src/app/mod.rs`: `TrayState.notice`, override `tool_tip()`, поле `bp_notice_until`, `set_bp_notice()` при включении BP в settings-save, подстановка notice в `tray_state()`. Проверка: `cargo check` + `cargo test` зелёные
[ ] Шаг 4: Завершение — финальный аудит `cargo check + clippy + test`, ROADMAP → done, консервация `_STATE_.md`, `git diff --stat`. Проверка: все шаги `[x]`, репозиторий чист после последнего коммита

- **Текущий шаг (current_step):** Шаг 3
- **Следующий ход:** Добавить `TrayState.notice` в `src/tray.rs` (override `tool_tip()`) и `bp_notice_until`/`set_bp_notice()` в `src/app/mod.rs` (вызов при включении BP в settings-save, подстановка в `tray_state()`)
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress