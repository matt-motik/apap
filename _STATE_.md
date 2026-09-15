# Текущая микро-сессия

- **Задача из ROADMAP:** T1.0 — Система тем из TOML-файлов (ЭТАП 7)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/theme.rs` (новый модуль lib)
  - `src/lib.rs`
  - `src/settings.rs`
  - `src/app/mod.rs`
  - `src/app/ui_manager.rs`
  - `ui/app.slint`
  - `ui/settings.slint`
  - `ui/top_panel.slint`
- **Критерий успеха (Definition of Done):** `cargo test` + `cargo clippy` зелёные; темы загружаются из `themes/*.toml` (структура+HEX), UI-выбор с динамическим списком и блокировкой Save, `settings.toml theme = "<имя>"`, fallback Light без перезаписи средствами settings.toml, иконки следуют за `standard_palette`.

## Итерационный трекер

[x] Шаг 1: Создать `src/theme.rs` (ThemeData/ColorsData/StandardPalette/ThemeEntry/ThemeError, `load_from_file`, `parse_hex`, `DEFAULT_DARK_TOML`/`DEFAULT_LIGHT_TOML`, `create_default_themes`, `scan_themes_dir`) + регистрация `pub mod theme;` в `src/lib.rs` + 7 lib-тестов (§9). Проверка: cargo test theme:: зелёный.
[x] Шаг 2: `src/app/mod.rs`: `hex_color` → обёртка над `theme::parse_hex` (поведение/тесты `hex_color_*` сохраняются). Проверка: cargo test hex_color зелёный.
[x] Шаг 3: `ui/app.slint` + `ui/top_panel.slint`: добавить `in property <int> theme-palette` (app.slint), binding TopPanel `theme-palette: root.theme-palette`, top_panel: переименовать `theme`→`theme-palette` и заменить 8 мест `root.theme == 0` → `root.theme-palette == 0`. settings-theme(int) временно остаётся. Проверка: cargo check.
[x] Шаг 4: `ui/app.slint` + `ui/settings.slint`: добавить `theme-list-model`/`theme-current`/`theme-name`/`theme-description`/`theme-save-enabled` + callback `settings-theme-selected(string)`; ComboBox → `model`/`current-value`; два Text метаданных; Save → `enabled: root.theme-save-enabled`. Проверка: cargo check.
[x] Шаг 5: `src/app/mod.rs`: чистые `validate_colors(&ColorsData)` и `resolve_startup_theme(&str, &Path) -> (ThemeData, bool)` + 3 bin-теста (§9: ok/bad_hex/startup_fallback). Проверка: cargo test validate_colors / startup_fallback.
[ ] Шаг 6: Атомарный свап (одна сущность — тип поля; 3 файла): `src/settings.rs` (theme: String + `default_theme()="light"`, enum Theme удалить) + `src/app/ui_manager.rs` (`apply_theme(&ThemeData)`: ColorScheme по standard_palette, 22 цвета через validate_colors+hex_color, `set_theme_palette`; sync_settings_to_ui без `set_settings_theme`) + `src/app/mod.rs` (импорт Theme убрать, callback 19 удалить, settings-save → `apply_theme(&resolve_startup_theme(...))`). Проверка: cargo check + cargo clippy.
[ ] Шаг 7: `src/app/mod.rs`: поле `theme_selection: Option<String>` (+ theme_meta), `MusicApp::new` → `create_default_themes(config_dir()/themes)`, `init` → `resolve_startup_theme` + `apply_theme(&data)` + tray-notice при `was_fallback`. Проверка: cargo check.
[ ] Шаг 8: `src/app/mod.rs`: `on_open_settings` → populate (scan_themes_dir → list-model, theme-current, name/description, save-enabled); callback `settings-theme-selected` (§5.4/§6.3); settings-save commit (theme_selection → draft.theme → apply). Проверка: cargo check.
[ ] Шаг 9: `ui/app.slint` + `ui/settings.slint`: удалить `settings-theme(int)`, `settings-theme-changed(int)`, `set-theme(int)`, forwarding `set-theme(i) =>` (финальная очистка, последние ссылки убраны в Шаге 6-8). Проверка: cargo check + cargo clippy + cargo test (все).

- **Текущий шаг (current_step):** Шаг 6
- **Следующий ход:** Атомарный свап `Theme` → `String` (одна сущность, 3 файла): (1) `src/settings.rs` — `pub theme: String` с `#[serde(default = "default_theme")]` (`default_theme() -> String { "light".into() }`), удалить enum `Theme`, дефолт `Settings` → `theme: default_theme()`; (2) `src/app/ui_manager.rs` — `apply_theme(&ThemeData)`: ColorScheme по standard_palette, 22 цвета через `validate_colors`+`hex_color` (убрать `hex_color_lit`-хардкод), `set_theme_palette(0|1)`; `sync_settings_to_ui` — убрать `set_settings_theme`, ставить `set_theme_palette` по `settings.theme == "dark"`; (3) `src/app/mod.rs` — убрать Theme из импорта и из settings-save (строки ~965-968 → `apply_theme(&resolve_startup_theme(..., themes_dir).0)`), удалить блок callback 19 (on_settings_theme_changed), снять `#[allow(dead_code)]` с validate_colors/resolve_startup_theme. Проверка: cargo check + clippy.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress