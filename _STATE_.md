# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммится вместе с изменениями кода.

## Статус

- **Состояние:** `done` — названия полей инфо-панели выведены в конфиг (`info_labels`), коммит в процессе.

## Активная задача

Нет.

## Выполнено

### Названия полей инфо-панели в конфиг (`info_labels`)
- `Settings.info_labels: HashMap<String, String>` + `INFO_LABEL_KEYS` (13 ключей) + `default_info_labels()` (англ. дефолты)
- Методы `Settings::info_label(key)` (fallback: конфиг → дефолт → ключ) и `info_labels_ordered()`
- `TopPanel.info-labels: [string]` — модель вместо 13 захардкоженных строк, `InfoRow` берут `root.info-labels[0..12]`
- `AppWindow.info-labels` проброс; `set_info_labels(...)` в `sync_settings_to_ui()`
- Локализация — правкой `[info_labels]` в config.toml
- 2 теста (дефолты в порядке отображения, override/fallback); clippy без новых warning

## Изменяемые файлы

- `src/settings.rs` — поле, константы, default-функция, методы, 2 теста
- `src/app/ui_manager.rs` — `set_info_labels` в sync_settings_to_ui
- `ui/top_panel.slint` — свойство `info-labels` + индексация вместо литералов
- `ui/app.slint` — проброс `info-labels`
- `ROADMAP.md` — пункт 4.13
- `_STATE_.md`