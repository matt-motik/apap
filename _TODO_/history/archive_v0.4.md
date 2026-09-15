# Архив реализации: ЭТАП 7 — система тем из TOML (спека T1.0)

> Файл создан при чистке ROADMAP.md 2026-09-16.
> Все коммиты влиты в `main`. ЭТАП 7 закрыт полностью, открытых подзадач нет.
> Спека: `docs/spec_theme_v1.0.md` (префикс `T1.0`, согласована в чате 2026-09-15).

---

## 1. Итог ЭТАПА 7

- Темы загружаются из `themes/*.toml` (`ThemeData::load_from_file`, структурная
  валидация через serde: 22 поля `[colors]` + метаданные).
- `Settings.theme: String` (`#[serde(default = "default_theme")]`, дефолт
  `"light"`); enum `Theme` удалён полностью.
- `apply_theme(&ThemeData)` — цвета из TOML (хардкод удалён), `theme-palette`
  для иконок Следует за `standard_palette`, переключение без перезапуска.
- UI-выбор в диалоге: `ComboBox` (list/current-value), name/description,
  блокировка Save для невалидной/ненайденной темы.
- Fallback при ошибке загрузки на старте: Light визуально + тултип трея;
  `settings.toml` не перезаписывается (намерение пользователя сохраняется).
- Удалены унаследованные int-свойства `settings-theme` / `settings-theme-changed`
  / `set-theme(int)`.

## 2. Технический паспорт (шаг → коммит → что сделано)

| Шаг | Коммит | Дата | Технические детали |
| --- | --- | --- | --- |
| 1 | `c35c0ad` | 2026-09-15 | lib-модуль `src/theme.rs`: `StandardPalette`/`ColorsData`/`ThemeData`/`ThemeEntry`/`ThemeError`, `load_from_file`, `parse_hex`, `DEFAULT_DARK_TOML`/`DEFAULT_LIGHT_TOML`, `create_default_themes`, `scan_themes_dir`; регистрация `pub mod theme;` в `src/lib.rs`; 7 lib-тестов. |
| 2 | `c4e5174` | 2026-09-15 | bin-обёртка `hex_color(hex) -> Option<slint::Color>` над `parse_hex` (a==255 → `from_rgb_u8`, иначе `from_argb_u8`); прежние `hex_color_*` тесты сохранены. |
| 3 | `49d77d5` | 2026-09-15 | `theme-palette` (0=dark/1=light): `in property <int>` в app.slint, binding TopPanel, `root.theme == 0` → `root.theme-palette == 0` (8 мест). |
| 4 | `f63a007` | 2026-09-15 | UI выбора: `theme-list-model`/`theme-current`/`theme-name`/`theme-description`/`theme-save-enabled` + callback `settings-theme-selected(string)`; ComboBox model/current-value; 2 Text метаданных; Save `enabled`. |
| 5 | `d308bc5` | 2026-09-15 | bin: `validate_colors(&ColorsData) -> Result<(), ThemeError::InvalidHex{field}>` (22 hex через `parse_hex`); `resolve_startup_theme(name, dir) -> (ThemeData, bool)` — цепочка `<name>.toml` → `light.toml` → `DEFAULT_LIGHT_TOML`, чистая (не пишет settings.toml); 3 bin-теста. |
| 6 | `688b4bc` | 2026-09-15 | Атомарный свап enum→String: `Settings.theme: String`; `apply_theme(&ThemeData)` в ui_manager (FluentPalette.color_scheme по standard_palette, 22 цвета через `validate_colors`+`hex_color`); `sync_settings_to_ui` без `set_settings_theme`; settings-save → `apply_theme(&resolve_startup_theme(...))`. |
| 7–8 | `51f6075` | 2026-09-16 | Состояние диалога: поля `theme_selection: Option<String>` + `theme_meta: Option<ThemeMeta>`; `MusicApp::new` → `create_default_themes(config_dir()/themes)`; `init` → `resolve_startup_theme`+`apply_theme`+tray-notice при `was_fallback`; `on_open_settings` populate (§6.2); callback 19 `settings-theme-selected` (§6.3); settings-save коммит `theme_selection` → `settings.theme` (§6.4). Обобщён tray-notice: `bp_notice_until` → `tray_notice: Option<(Instant, String)>`. |
| 9 | `c46e7e4` | 2026-09-16 | Удалены `settings-theme(int)`/`settings-theme-changed(int)` из app.slint; верхняя панель `theme-palette: root.theme-palette` (финальная очистка §7/§8.3). |
| — | `9fc0cf4` | 2026-09-16 | chore: статус T1.0 → ✅ в ROADMAP, `_STATE_.md` → done. |

## 3. Ключевые структуры данных и константы

- **`ColorsData`** — контракт ровно **22 полей** `String` (hex): `bg_window,
  bg_surface, bg_toolbar, bg_elevated, bg_overlay, border_subtle, border_default,
  text_primary, text_secondary, text_tertiary, text_dim, text_on_accent,
  text_error, accent, accent_container, accent_on, surface_hover, surface_active,
  surface_selected, viz_1, viz_2, viz_3` (сверено с `ui/theme.slint`).
- **`StandardPalette`** — `#[serde(rename_all = "lowercase")]`, варианты
  `Dark`/`Light`; управляет `FluentPalette.color_scheme` и набором иконок.
- **`ThemeData { name, description: Option<String>, standard_palette, colors }`** —
  `load_from_file(path)` проверяет только структуру (serde); HEX не парсится.
- **`ThemeEntry { file_stem, name, description, valid }`** — результат
  `scan_themes_dir`: только `*.toml` верхнего уровня, сортировка
  лексикографическая case-insensitive; вложенные папки игнорируются.
- **`parse_hex(hex) -> Option<(a, r, g, b)>`** — `#RRGGBB`/`#AARRGGBB`,
  все байты `is_ascii_hexdigit`, отказ → `None` (6-знач. альфа = 255).
- **`DEFAULT_DARK_TOML` / `DEFAULT_LIGHT_TOML`** — полные шаблоны (22 поля);
  значения взяты из прежних dark/light веток `apply_theme()`.
- **`create_default_themes(dir)`** — `create_dir_all` + запись `dark.toml`/
  `light.toml` только при отсутствии (не перезаписывает, воссоздаёт удалённые).
- **`resolve_startup_theme(name, dir) -> (ThemeData, bool)`** — цепочка fallback
  `<name>.toml` (структура+HEX) → `light.toml` → `DEFAULT_LIGHT_TOML`;
  `was_fallback` → тултип трея; settings.toml НЕ пишет.
- **`ThemeMeta { name, description, valid }`** (bin) — метаданные темы для UI
  диалога; `valid` = структура + HEX.
- **Tray-notice**: `tray_notice: Option<(Instant, String)>` — генерализация
  прежнего `bp_notice_until`; дедлайн `Instant + 5 c` (текст произвольный:
  `tray::BP_NOTICE_TEXT` и сообщение fallback темы).
- **Уведомление fallback**: «Тема "<имя>" не загружена, применена светлая тема».

## 4. Реализованные сценарии (§6 спеки)

1. Старт: `create_default_themes` → `resolve_startup_theme(settings.theme)` →
   `apply_theme`; ошибка → Light визуально + тултип трея, `settings.toml`
   не трогается.
2. Открытие диалога: `populate_theme_ui` — `scan_themes_dir` → `theme-list-model`,
   `theme-current` = `settings.theme`, метаданные текущей темы,
   `theme-save-enabled` по валидности. Удалённый файл → «(тема не найдена)»,
   Save заблокирован. Сброс `theme_selection = None`.
3. Выбор в ComboBox: `settings-theme-selected(name)` → `load_theme_meta`
   (структура+HEX) → name/description/`theme-save-enabled`; в draft не пишет;
   валидная → `theme_selection = Some(name)`; ошибка → «(не удалось загрузить
   метаданные)», Save заблокирован.
4. «Сохранить»: `theme_selection` → `settings.theme` → diff-apply → commit →
   `apply_theme(&resolve_startup_theme(...))` без перезапуска.
5. Следующий запуск: загружается последняя выбранная тема (п.1).

## 5. Верификация

- `cargo test`: **146 lib + 13 bin** зелёные (добавлены: 7 lib-тестов `theme.rs`,
  bin — `test_validate_colors_ok`/`test_validate_colors_bad_hex`/
  `test_startup_fallback_missing_file`/`test_theme_field_default` +
  сохранённые `hex_color_*`).
- `cargo clippy`: 0 новых варнингов в изменённых файлах (остались только
  прежние `if_same_then_else`/`while_let_loop` и колоночные линты).
- 22 цвета сверены с `ui/theme.slint` (контракт §2.1).

## 6. Что осталось / следующий шаг

- **ЭТАП 7 закрыт полностью** (открытых подзадач T1.0 нет).
- Из предыдущих этапов актуальны два пункта V5.1: `V5.1-8.6.2` (ручная
  DoP-верификация на железе) и `V5.1-11` (сквозное документирование).
- Следующая задача — из `_TODO_/` или напрямую из чата (см. `_STATE_.md`).