# ТЗ: Система тем на основе TOML-файлов

- **Префикс спеки:** `T1.0` (реестр префиксов — в шапке `ROADMAP.md`).
- **Статус:** согласовано в чате 2026-09-15, ожидает реализации.
- **Этап:** 7 (Рефакторинг и оптимизация). **Приоритет:** 🟢 средний.
- **Источник:** `_TODO_/spec_theme_v1.0.md` (обработано, перенесено в `docs/`, оригинал отработан).

## 1. Структура файлов

```
.config/music_player/
├── settings.toml          # текущий файл настроек
└── themes/                # папка для тем
    ├── dark.toml          # дефолтная тёмная тема (создаётся приложением)
    ├── light.toml         # дефолтная светлая тема (создаётся приложением)
    └── custom.toml        # пример пользовательской темы (создаёт пользователь)
```

Правила:
- Папка `themes/` создаётся при первом запуске, если её нет.
- Файлы `dark.toml` и `light.toml` создаются приложением при запуске, если
  они отсутствуют. Если файл уже существует — приложение его **не
  перезаписывает** (в т.ч. когда пользователь удалил файл вручную — он
  воссоздаётся из дефолтного шаблона при следующем запуске).
- Пользователь может редактировать любые темы напрямую (файлы `themes/*.toml`).
- `custom.toml` не создаётся приложением — это только пример для пользователя.

## 2. Формат TOML-файла темы

### 2.1. Инвентаризация цветов

Подтверждён полный список app-specific цветов (сверено с `ui/theme.slint`
и `apply_theme()` в `src/app/ui_manager.rs`). Ровно **22 поля**:

```
bg_window, bg_surface, bg_toolbar, bg_elevated, bg_overlay,
border_subtle, border_default,
text_primary, text_secondary, text_tertiary, text_dim,
text_on_accent, text_error,
accent, accent_container, accent_on,
surface_hover, surface_active, surface_selected,
viz_1, viz_2, viz_3
```

Этот список — контракт для структуры `ColorsData`, дефолтных
`dark.toml`/`light.toml` и примеров в этом ТЗ. Виджетный `Palette`
(`tableview_fork.slint`, std-widgets) **НЕ входит** в скоуп данной спеки —
отдельная задача «Темизация виджетного Palette».

### 2.2. ThemeData

```
ThemeData {
    name: String,                      // отображается под ComboBox
    description: Option<String>,       // None → пустая строка в UI
    standard_palette: StandardPalette, // "dark" | "light"
    colors: ColorsData,                // ровно 22 поля (String hex)
}
```

- `StandardPalette` — enum с `#[serde(rename_all = "lowercase")]`
  (варианты `Dark`/`Light`).
- Блок `[colors]` обязателен и содержит все 22 поля.
- `standard_palette` определяет `FluentPalette.color_scheme` **и** набор
  иконок верхней панели (см. §7).

### 2.3. Разделение структурной и HEX-валидации

Либ-крейт (`music_player_rs::theme`) **НЕ зависит от slint**. Отсюда два
уровня проверки:

1. `ThemeData::load_from_file(path)` (lib) — проверяет **только структуру**:
   наличие всех 22 полей блока `[colors]` и полей метаданных. Отсутствие
   поля/опечатка имени → ошибка (достигается самим serde). HEX-строки на
   этом уровне **не парсятся**.
2. `parse_hex(hex: &str) -> Option<(u8, u8, u8, u8)>` (lib, чистая функция)
   — разбор `#RRGGBB` / `#AARRGGBB`. Возвращает кортеж `(a, r, g, b)`.
   Заменяет логику текущего `hex_color()` из `src/app/mod.rs`.
3. `validate_colors(&ColorsData) -> Result<(), ThemeError>` (bin-крейт) —
   прогоняет все 22 hex-строки через `parse_hex`; любой отказ → ошибка
   (имя поля для диагностики).
4. `hex_color(hex: &str) -> Option<slint::Color>` (bin) — обёртка над
   `parse_hex`: при `a == 255` → `slint::Color::from_rgb_u8(r, g, b)`,
   иначе → `from_argb_u8(a, r, g, b)`.

Пример файла темы на базе **Dark** (полный, та же структура для Light со
значениями из текущей light-ветки `apply_theme()`):

```toml
name = "Dark"
description = "Системная тёмная тема"
standard_palette = "dark"

[colors]
bg_window = "#121018"
bg_surface = "#1a1720"
bg_toolbar = "#211e28"
bg_elevated = "#252230"
bg_overlay = "#00000088"
border_subtle = "#2d2a38"
border_default = "#3a3645"
text_primary = "#e6e1ec"
text_secondary = "#a9a3b8"
text_tertiary = "#7c7690"
text_dim = "#5c5670"
text_on_accent = "#ffffff"
text_error = "#f2b8b5"
accent = "#d0bcff"
accent_container = "#4f378b"
accent_on = "#eaddff"
surface_hover = "#322e3c"
surface_active = "#3a2f1f"
surface_selected = "#2d2a38"
viz_1 = "#d35400"
viz_2 = "#f1c40f"
viz_3 = "#e74c3c"
```

## 3. Резервированные имена тем

- Список доступных тем формируется из файлов `themes/*.toml`; имя темы =
  имя файла без расширения `.toml`.
- Имена `dark` и `light` зарезервированы за приложением:
  приложение гарантирует их наличие (создаёт при отсутствии), но не
  перезаписывает существующие; пользовательские правки этих файлов
  применяются.
- Создание/редактирование тем из UI **не предусмотрено** — только через
  файловую систему. Валидации имён нет.
- Вложенные папки внутри `themes/` **не читаются** (только `*.toml`
  верхнего уровня).

## 4. Изменения в Settings

- Поле `theme: String` (в TOML: `theme = "<имя файла без .toml>"`).
  `#[serde(default = "default_theme")]`, где
  `default_theme() -> String { "light".into() }`.
- Enum `Theme` **удаляется** вместе со всеми использованиями
  (`settings.rs`, `ui_manager.rs`, `app/mod.rs`).
- Обратная совместимость не требуется: при чтении старого `settings.toml`
  со значением `theme = "dark"` serde бесшумно десериализует строку —
  миграция бесплатна (старый конфиг можно удалить и пересоздать).
- Поля `theme_name`/`theme_description` в `Settings` **не добавляются** —
  это метаданные темы, читаются из TOML при загрузке.

## 5. UI выбора темы (диалог настроек)

### 5.1. ComboBox с динамической моделью

- `in property <[string]> theme-list-model` — имена файлов без `.toml`,
  сортировка лексикографическая, case-insensitive.
- `in property <string> theme-current` — имя текущей выбранной темы.
- В `settings.slint`: `model: root.theme-list-model`,
  `current-value: root.theme-current` (вместо `current-index` — индекс
  нестабилен: файлы добавляются/удаляются).
- Callback `set-theme(int)` заменяется на `settings-theme-selected(string)`;
  проброс в `app.slint` переименовывается аналогично.
- Модель заполняется из `scan_themes_dir()` при открытии диалога (см. §6.2).
- Если `theme-current` отсутствует в модели (файл текущей темы удалён) —
  ComboBox не находит совпадения; под списком показывается
  **«(тема не найдена)»**, кнопка «Сохранить» заблокирована. Имя в модель
  принудительно не добавляется (честный сценарий).

### 5.2. Метаданные темы под ComboBox

Размещение (сразу под ComboBox, без отдельного подраздела):

```
[ Тема: [Dark ▼] ]
Deep Dark
Тема на базе системной Dark...
```

- `in property <string> theme-name` — из `ThemeData::name`.
- `in property <string> theme-description` — из `ThemeData::description`
  (`None` → пустая строка).
- Падение чтения метаданных → `theme-name` = **«(не удалось загрузить
  метаданные)»**, `theme-description` = "".
- Два `Text`: name — `Colors.text-secondary`, description —
  `Colors.text-tertiary`, `font-size: 11px`.

### 5.3. Блокировка «Сохранить»

- `in property <bool> theme-save-enabled`.
- Кнопка «Сохранить»: `enabled: root.theme-save-enabled`.
- «Сохранить» записывает тему **только при валидной выбранной теме**; при
  невалидной/ненайденной — кнопка заблокирована.
- При открытии диалога: `true`, если текущая тема валидна; иначе `false`
  (например, пользователь сломал файл при работающем приложении).

### 5.4. Callbacks (замена settings-theme-changed)

- `settings-theme-selected(string)` — при выборе в ComboBox:
  Rust загружает `ThemeData`, обновляет `theme-name`/`theme-description`/
  `theme-save-enabled`. В настройки (draft/`settings.toml`) **не пишет**.
  Выбранное имя хранится в транзитном поле
  `theme_selection: Option<String>` структуры `MusicApp`.
- Существующий `settings-save` — при нажатии «Сохранить»: если выбранная
  тема валидна — записывает `theme_selection` в draft
  (`settings_mut().theme`), применяет `apply_theme()`, сохраняет через
  существующий механизм commit-draft (`a.settings.save()`). Если невалидна —
  недостижимо (кнопка заблокирована).
- `settings-theme-changed(int)` **удаляется**.

## 6. Логика загрузки тем

1. **При старте приложения:**
   - Прочитать `theme` из `settings.toml`.
   - Загрузить `themes/<theme>.toml` и провалидировать (структура + HEX).
   - Успех → применить через `apply_theme()`.
   - Ошибка → warning в лог, применить дефолтную Light **визуально** (для
     текущего запуска), уведомить пользователя (тултип трея ~5 с:
     «Тема "<имя>" не загружена, применена светлая тема» — существующий
     механизм tray-notice, как у bit-perfect). Значение `theme` в
     `settings.toml` **НЕ перезаписывается** — сохраняется намерение
     пользователя; после исправления TOML и перезапуска тема применится.
2. **При открытии диалога настроек** (существующий `ui.on_open_settings`):
   - Сканировать `themes/*.toml` → `theme-list-model` (сортировка §5.1).
   - Отметить `theme-current` из `settings.toml`.
   - Заполнить `theme-name`/`theme-description` по текущей теме,
     `theme-save-enabled` по валидности (§5.3).
   - Удалённый файл текущей темы → «(тема не найдена)», Save заблокирован.
3. **При выборе темы в ComboBox** (`settings-theme-selected`):
   - Загрузить TOML, валидировать (структура + HEX).
   - Успех → показать name/description, `theme-save-enabled = true`,
     `theme_selection = <имя>`.
   - Ошибка → «(не удалось загрузить метаданные)»,
     `theme-save-enabled = false`. В настройки не пишет.
4. **При нажатии «Сохранить»:**
   - Тема валидна → `theme_selection` в `draft.theme`, commit-draft →
     `settings.toml`, `apply_theme()`, синхронизация `theme-palette`.
     Без перезапуска.
   - Тема невалидна → недостижимо (кнопка заблокирована, §5.3).
5. Следующий запуск — загружается последняя выбранная тема (см. п.1).

## 7. Иконки и Slint-состояние (перенос int-свойства)

- Набор иконок `top_panel` определяется **только** `standard_palette`:
  dark → `icons/dark/`, light → `icons/light/`.
- `app.slint`: свойство `in property <int> settings-theme: 0` заменяется на
  `in property <int> theme-palette: 0` (0=dark, 1=light).
- `top_panel.slint`: собственное `in property <int> theme: 0` переименовывается
  в `in property <int> theme-palette: 0`; **все** вхождения
  `root.theme == 0` заменяются на `root.theme-palette == 0`
  (7 мест: prev/pause/play/stop/next/repeat/shuffle + muted/volume).
- `app.slint`: привязка верхней панели `theme: root.settings-theme`
  → `theme-palette: root.theme-palette`.
- `settings.slint`: int-свойство `theme` удаляется (ComboBox уходит на
  `current-value` со строковой моделью); `set-theme(int)` →
  `settings-theme-selected(string)`; проброс в `app.slint`
  (`set-theme(i) => ...`) переименовывается аналогично.
- `apply_theme()` синхронизирует `set_theme_palette(0|1)` по
  `standard_palette`.
- Новые свойства `app.slint`: `theme-list-model`, `theme-current`,
  `theme-name`, `theme-description`, `theme-save-enabled`, `theme-palette`.

## 8. Файлы и модули

### 8.1. Новый модуль `src/theme.rs` (lib)

- Структуры: `ThemeData`, `ColorsData` (22 поля `String`), `StandardPalette`
  (`#[serde(rename_all = "lowercase")]`), `ThemeEntry`, `ThemeError`.
- `ThemeData::load_from_file(path) -> Result<Self, ThemeError>` — структурная
  валидация через serde; HEX не проверяет (§2.3).
- `parse_hex(hex: &str) -> Option<(u8, u8, u8, u8)>` — чистая функция (§2.3).
- `const DEFAULT_DARK_TOML: &str` / `DEFAULT_LIGHT_TOML: &str` — полные
  шаблоны (22 поля; значения из текущих dark/light веток `apply_theme()`).
- `create_default_themes(dir: &Path) -> std::io::Result<()>` — создаёт папку
  и файлы `dark.toml`/`light.toml`, если их нет; существующие **не
  перезаписывает**.
- `scan_themes_dir(dir: &Path) -> Vec<ThemeEntry>` — только `*.toml`
  верхнего уровня; для каждого: `file_stem`, `name`, `description`, `valid`.
  Сортировка: лексикографическая, case-insensitive.
- Модуль регистрируется в `src/lib.rs`.

### 8.2. bin-крейт

- `validate_colors(&ColorsData) -> Result<(), ThemeError>` — HEX всех 22
  полей через `parse_hex` (§2.3).
- `hex_color(hex: &str) -> Option<slint::Color>` — обёртка над `parse_hex`
  (логика переносится из `app/mod.rs`, тесты сохраняются).
- `apply_theme(&self, theme: &ThemeData)` (в `ui_manager.rs`) —
  `FluentPalette.color_scheme` по `standard_palette`, 22 цвета из
  `theme.colors` через `hex_color` (сначала `validate_colors`),
  `set_theme_palette(0|1)` по `standard_palette`. Хардкод удаляется;
  `hex_color_lit()` остаётся только если нужен ещё где-то.
- `resolve_startup_theme(name: &str, themes_dir: &Path) -> (ThemeData, bool)`
  — чистая функция без GUI: пробует `<name>.toml` (структура + HEX); при
  ошибке → `light.toml`; при ошибке → парсинг `DEFAULT_LIGHT_TOML`.
  Возвращает `(данные_темы, was_fallback)`. **settings.toml не пишет.**
  Вызывается из `MusicApp::init()` по §6.1 (лог-warning + тултип трея при
  `was_fallback`).
- `MusicApp::new()`: после `SettingsStore::load()` →
  `create_default_themes(config_dir().join("themes"))`.
- `MusicApp::init()`: `resolve_startup_theme(settings.theme, &themes_dir)` +
  `apply_theme(&data)`; уведомление по §6.1.
- Состояние диалога в `MusicApp`: `theme_selection: Option<String>`,
  `theme_meta` (name/description/valid) для UI.
- `on_open_settings`: populate темы (§6.2). `on_settings_theme_selected`:
  §6.3. `settings-save`: §6.4. `on_settings_theme_changed(int)` удаляется.

### 8.3. Slint

- `ui/theme.slint` — без изменений (22 свойства = контракт `ColorsData`).
- `ui/app.slint` — новые свойства/callback (§5, §7); удалить
  `settings-theme` (int) и `settings-theme-changed(int)`.
- `ui/settings.slint` — ComboBox `model`/`current-value` (§5.1), два `Text`
  с метаданными (§5.2), `enabled` у кнопки «Сохранить» (§5.3),
  callback `settings-theme-selected(string)`.
- `ui/top_panel.slint` — `root.theme == 0` → `root.theme-palette == 0` (§7).

## 9. Тестирование

Lib (`src/theme.rs`):

- `test_load_default_dark` — загрузка `DEFAULT_DARK_TOML` проходит.
- `test_load_default_light` — загрузка `DEFAULT_LIGHT_TOML` проходит.
- `test_missing_color_field` — отсутствие любого из 22 полей → ошибка.
- `test_scan_themes_dir_ignores_subdirs` — вложенные папки игнорируются.
- `test_create_default_themes_no_overwrite` — существующие файлы не
  перезаписываются.
- `test_parse_hex_valid` / `test_parse_hex_invalid` — корректные/некорректные
  HEX-строки (перенос логики из текущих `hex_color_*` тестов).

Bin (`src/app/mod.rs`):

- `test_validate_colors_ok` — все 22 поля валидны.
- `test_validate_colors_bad_hex` — одно невалидное поле → ошибка с именем.
- `test_startup_fallback_missing_file` — главный сценарий: в temp-каталоге
  `themes_dir` без `ghost.toml` вызываем
  `resolve_startup_theme("ghost", dir)` → возвращаются Light-данные
  (`standard_palette = light`, цвета из `light.toml`/`DEFAULT_LIGHT_TOML`),
  `was_fallback = true`; `settings.toml` при этом не создан/не изменён
  (функция чистая — побочных записей нет). Намерение пользователя в
  settings сохраняется.
- `test_theme_field_default` — отсутствие `theme` в settings.toml → "light".

## Критерии готовности

1. Темы загружаются из TOML-файлов (`ThemeData::load_from_file`).
2. Пользователь может добавить тему созданием файла в `themes/` — она
   появляется в списке.
3. Имена `dark` и `light` закреплены: файлы восстанавливаются при отсутствии,
   не перезаписываются при наличии.
4. Выбор темы сохраняется в `settings.toml` как `theme = "<имя>"`.
5. UI отображает список тем + name/description выбранной.
6. Хардкод цветов удалён из `apply_theme()`, цвета берутся из `ThemeData`.
7. Иконки следуют за `standard_palette`; переключение без перезапуска.
8. Падение загрузки темы на старте не ломает приложение (Light + тултип),
   намерение пользователя в `settings.toml` сохраняется.

## Заметки

- Дефолт при отсутствии поля `theme` — `"light"`.
- Fallback при ошибке загрузки — Light визуально; при недоступности
  `light.toml` — из `DEFAULT_LIGHT_TOML` в коде.
- Логирование ошибок загрузки тем обязательно.
- Префикс `T1.0` регистрируется в шапке `ROADMAP.md`.