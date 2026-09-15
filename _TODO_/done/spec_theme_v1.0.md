# Задача: Рефакторинг системы тем — вынос цветов в TOML-файлы
Приоритет: средний
Этап: 7 (Рефакторинг и оптимизация)

## Цель
Заменить хардкод цветов в `apply_theme()` на загрузку из TOML-файлов тем, чтобы пользователи могли добавлять свои темы созданием новых файлов.

## Требования

### 1. Структура файлов
```
.config/music_player/
├── settings.toml          # текущий файл настроек
└── themes/                # папка для тем
    ├── dark.toml          # дефолтная тема (создаётся приложением)
    ├── light.toml         # дефолтная тема (создаётся приложением)
    └── custom.toml        # пример пользовательской темы (создаётся пользователем)
```

Правила:
- Папка themes/ создаётся при первом запуске, если её нет.
- Файлы dark.toml и light.toml создаются приложением при запуске,
  если они отсутствуют. Если файл уже существует — приложение его не перезаписывает.
  Это касается и случая, когда пользователь удалил файл вручную.
- Пользователь может редактировать dark.toml и light.toml.
- custom.toml — не обязателен, приведён как пример. Пользователь создаёт его сам.


### 2. Формат TOML-файла темы

#### 2.1. Инвентаризация цветов (подготовительный шаг)

Перед описанием формата темы нужно собрать полный список app-specific цветов:
- Пройти по apply_theme() и выписать все хардкод-значения.
- Пройти по ui/*.slint и найти цвета, не входящие в Palette Slint.
- Свести их в единый список с именами и дефолтными значениями.

Результат: полный список полей ColorsData (22 поля):
bg_window, bg_surface, bg_toolbar, bg_elevated, bg_overlay,
border_subtle, border_default,
text_primary, text_secondary, text_tertiary, text_dim,
text_on_accent, text_error,
accent, accent_container, accent_on,
surface_hover, surface_active, surface_selected,
viz_1, viz_2, viz_3. УТОЧНИ!

Результат: полный список полей ColorsData.
Этот список попадает в структуру ColorsData, в дефолтные dark.toml / light.toml
и в примеры TOML в этом ТЗ.

#### 2.2. ThemeData

`ThemeData` — это структура темы, которая содержит:
- метаданные (name, description),
- выбор базовой палитры (standard_palette),
- обязательный блок [colors] с app-specific цветами.

Структура TOML файла темы:

    Поле standard_palette: "dark", "light".
    Блок [colors] (обязателен):  со всеми 22 app-specific цветами.
    
Логика загрузки (ThemeData -> Slint):

    Читаем standard_palette.
    Если "dark": применяем FluentPalette.color_scheme = Dark.
    Если "light": применяем FluentPalette.color_scheme = Light.
    Всегда: читаем [colors] и применяем 22 цвета в глобал Colors.
    Если блок [colors] отсутствует или в нём не хватает хотя бы одного поля —
    ошибка загрузки, fallback на дефолтную Light, уведомление пользователю.

Пример файла темы на базе Dark:
(в примерах список сокращён через "# ...";
в реальном файле перечисляются все app-specific цвета):
```
    name = "Deep Dark"
    description = "Тема на базе системной Dark со своими цветами визуализатора"
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

### 3. Резервированные имена тем

Приложение не создаёт и не редактирует темы из UI.
Пользователь работает с файлами в themes/ напрямую, через файловую систему.

Правила:
- Список доступных тем формируется из файлов themes/*.toml.
  Имя темы = имя файла без расширения .toml.
- Имена dark и light зарезервированы за приложением:
    - приложение гарантирует наличие dark.toml и light.toml
      (создаёт при отсутствии, см. раздел 1);
    - пользователь может редактировать эти файлы, и приложение
      применит его изменения;
    - если файл повреждён или невалиден — ошибка загрузки,
      fallback на дефолтную Light (см. раздел 2);
- Отдельной валидации имён при создании/сохранении нет,
  так как создание тем из UI не предусмотрено.
- Если пользователь создал тему с именем, отличным от dark/light,
  она появляется в списке доступных тем автоматически.
- Вложенные папки тем не читаем


### 4. Изменения в Settings
Добавить поле в `settings.toml`:
```toml
theme = "dark"  # или "light", или "mycustom" (имя файла без .toml)
```

Текущий theme: Theme enum заменить на строковый тип (String).

Обратная совместимость не требуется: приложение в разработке,
старый конфиг удали и пересоздай.

Если поле theme отсутствует — использовать "light" как дефолт.

### 5. UI для выбора темы
В диалоге настроек добавить:
- Выпадающий список тем: все файлы themes/*.toml.
- В списке отображается имя файла без .toml.
- В списке выбрана текущая тема (из settings.toml).
- При выборе темы в комбобоксе выполняется валидация:
  файл читается и успешно десериализуется как ThemeData.
- Под выпадающим списком — name из метаданных выбранной темы.
- Под именем — description из метаданных.
  Если description отсутствует — строка пустая.
- Если метаданные не читаются — под списком
  "(не удалось загрузить метаданные)".
- Если выбранная тема невалидна — кнопка "Сохранить" заблокирована.
- Кнопка "Сохранить" записывает выбранную тему в settings.toml
  как theme = "<имя файла без .toml>".
- Список формируется при открытии диалога.

### 6. Логика загрузки тем
1. При старте приложения:
   - Прочитать theme из settings.toml.
   - Загрузить themes/<theme>.toml и валидировать.
   - Успех → применить через apply_theme().
   - Ошибка → warning в лог, fallback на дефолтную Light,
     уведомление, записать theme = "light" в settings.toml.
2. При открытии диалога настроек:
   - Сканировать themes/*.toml.
   - Построить список: [<имя файла без .toml>, ...].
   - Отметить текущую тему из settings.toml как выбранную.
   - Если текущая тема из settings.toml невалидна на момент открытия диалога:
     в списке она выбрана, под списком "(не удалось загрузить метаданные)",
     кнопка "Сохранить" заблокирована.
3. При выборе темы в комбобоксе:
   - Загрузить TOML-файл, валидировать (десериализация ThemeData).
   - Успех → показать name/description, разблокировать "Сохранить".
   - Ошибка → "(не удалось загрузить метаданные)",
     заблокировать "Сохранить".
4. При нажатии "Сохранить":
   - Записать theme = "<имя файла без .toml>" в settings.toml.
   - Применить цвета через apply_theme().
5. При следующем запуске:
   - Загружается последняя активная тема (см. п. 1).

### 7. Технические детали

#### Новые файлы/модули:
- `src/theme.rs` — структура ThemeData, ColorsData, StandardPalette,
  загрузка TOML, дефолтные шаблоны dark/light, сканирование themes/.
- `.config/music_player/themes/dark.toml` — дефолтная dark тема (создаётся приложением)
- `.config/music_player/themes/light.toml` — дефолтная light тема (создаётся приложением)

#### Структуры (src/theme.rs):

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ThemeData {
        pub name: String,
        pub description: Option<String>,
        pub standard_palette: StandardPalette,
        pub colors: ColorsData,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum StandardPalette {
        Dark,
        Light,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ColorsData {
        pub bg_window: String,
        pub bg_surface: String,
        pub bg_toolbar: String,
        pub bg_elevated: String,
        pub bg_overlay: String,
        pub border_subtle: String,
        pub border_default: String,
        pub text_primary: String,
        pub text_secondary: String,
        pub text_tertiary: String,
        pub text_dim: String,
        pub text_on_accent: String,
        pub text_error: String,
        pub accent: String,
        pub accent_container: String,
        pub accent_on: String,
        pub surface_hover: String,
        pub surface_active: String,
        pub surface_selected: String,
        pub viz_1: String,
        pub viz_2: String,
        pub viz_3: String,
    }

#### Публичное API (src/theme.rs):

    impl ThemeData {
        /// Загрузить и провалидировать TOML-файл темы.
        pub fn load_from_file(path: &Path) -> Result<Self, ThemeError>;
    }

    /// Шаблоны дефолтных тем (используются для создания файлов при отсутствии).
    pub const DEFAULT_DARK_TOML: &str;
    pub const DEFAULT_LIGHT_TOML: &str;

    /// Создать themes/ и дефолтные dark.toml/light.toml, если их нет.
    /// Существующие файлы не перезаписываются.
    pub fn create_default_themes(dir: &Path) -> std::io::Result<()>;

    /// Сканировать themes/*.toml (вложенные папки игнорировать).
    /// Возвращает список записей с именем файла и метаданными/статусом.
    pub fn scan_themes_dir(dir: &Path) -> Vec<ThemeEntry>;

    pub struct ThemeEntry {
        pub file_stem: String,             // имя файла без .toml
        pub name: Option<String>,          // из метаданных, None если не читается
        pub description: Option<String>,
        pub valid: bool,                   // прошла ли полная валидация ThemeData
    }

#### Изменения в существующих файлах:

- `src/settings.rs`:
    - Theme enum заменить на строковый тип.
    - Поле theme: String (в TOML — theme = "<имя файла без .toml>").
    - serde-дефолт: "light".
    - Поля theme_name и theme_description в Settings НЕ добавляются —
      это метаданные темы, читаются из TOML при загрузке.

- `src/app/ui_manager.rs`:
    - apply_theme() переписать: принимает &ThemeData, применяет
      FluentPalette.color_scheme по standard_palette и 22 цвета
      из colors в глобал Colors. Хардкод удалить.
    - Использовать существующий hex_color() для парсинга строк.

- `src/app/mod.rs`:
    - В MusicApp::new(): вызвать theme::create_default_themes() после SettingsStore::load().
    - В MusicApp::init(): прочитать theme из Settings, загрузить
      themes/<theme>.toml, применить через apply_theme().
    - При ошибке — fallback на дефолтную Light (см. раздел 6.1).

- `ui/settings.slint`:
    - Добавить ComboBox для выбора темы (модель — список имён файлов).
    - Добавить Text для name выбранной темы (из ThemeData).
    - Добавить Text для description (из ThemeData).
    - Добавить блокировку кнопки "Сохранить", если выбранная тема невалидна.

#### Пути и крейты:

Проект — один крейт `music_player_rs` с `src/lib.rs` (библиотека) и
`src/main.rs` (бинарник). `src/theme.rs` и `src/settings.rs` — на стороне
библиотеки, доступны как `music_player_rs::theme` и `music_player_rs::settings`.
`src/app/*` — на стороне бинарника.

## План работ

### Этап 1: Модуль theme.rs — структуры и загрузка
- [ ] Создать `src/theme.rs` в крейте `music_player_rs`
- [ ] Определить `StandardPalette` (Dark | Light) с serde rename_all = "lowercase"
- [ ] Определить `ColorsData` (22 поля, все String)
- [ ] Определить `ThemeData` (name, description: Option<String>, standard_palette, colors)
- [ ] Определить `ThemeError` и `ThemeEntry`
- [ ] Реализовать `ThemeData::load_from_file(path) -> Result<Self, ThemeError>`
- [ ] Провалидировать все 22 цвета через существующий `hex_color()` (в app/mod.rs)

### Этап 2: Шаблоны дефолтных тем
- [ ] Определить `DEFAULT_DARK_TOML: &str` — содержимое dark.toml
- [ ] Определить `DEFAULT_LIGHT_TOML: &str` — содержимое light.toml
- [ ] Реализовать `create_default_themes(dir) -> io::Result<()>`:
      создаёт папку themes/ и файлы dark.toml/light.toml, если их нет.
      Существующие файлы не перезаписываются.
- [ ] Реализовать `scan_themes_dir(dir) -> Vec<ThemeEntry>`:
      только файлы *.toml, вложенные папки игнорируются.
      Для каждого — имя файла без .toml, name/description (если читаются), valid.

### Этап 3: Settings
- [ ] В `src/settings.rs` заменить `Theme` enum на `theme: String`
- [ ] `#[serde(default = "default_theme")]`, где `default_theme() -> "light".into()`
- [ ] Удалить `Theme` enum и все его использования
- [ ] Убедиться, что `SettingsStore::load()` читает/пишет поле `theme` как строку

### Этап 4: apply_theme()
- [ ] В `src/app/ui_manager.rs` переписать `apply_theme(&self, theme: &ThemeData)`
- [ ] Установить `FluentPalette.color_scheme` по `theme.standard_palette`
- [ ] Применить 22 цвета из `theme.colors` в глобал `Colors` через `hex_color()`
- [ ] Удалить хардкод цветов и `hex_color_lit()` для палитры
- [ ] `hex_color_lit()` оставить только там, где он ещё нужен (если нужен)

### Этап 5: Интеграция в mod.rs
- [ ] В `MusicApp::new()`: после `SettingsStore::load()` вызвать
      `theme::create_default_themes(&config_dir().join("themes"))`
- [ ] В `MusicApp::init()`: прочитать `settings.theme`, загрузить
      `themes/<theme>.toml` через `ThemeData::load_from_file`,
      применить через `apply_theme(&theme_data)`
- [ ] При ошибке: warning в лог, fallback на дефолтную Light
      (загрузить `themes/light.toml`, если он тоже невалиден — из `DEFAULT_LIGHT_TOML`),
      уведомление пользователю, записать `theme = "light"` в settings.toml

### Этап 6: UI выбора темы
- [ ] В `ui/settings.slint` добавить ComboBox со списком имён файлов тем
- [ ] Добавить Text для name выбранной темы (из ThemeData)
- [ ] Добавить Text для description (пустая строка, если None)
- [ ] Добавить блокировку кнопки "Сохранить", если выбранная тема невалидна
- [ ] В `MusicApp` добавить поля состояния диалога:
      список тем, выбранная тема, статус валидации, name/description
- [ ] Колбэк выбора темы: загрузить TOML, провалидировать,
      обновить name/description/статус, переключить блокировку
- [ ] Колбэк "Сохранить": записать `theme = "<file_stem>"` и применить тему
- [ ] При открытии диалога: сканировать `themes/`, отметить текущую тему,
      провалидировать её, обновить UI

### Этап 7: Тестирование
- [ ] `test_load_default_dark` — загрузка `DEFAULT_DARK_TOML` проходит
- [ ] `test_load_default_light` — загрузка `DEFAULT_LIGHT_TOML` проходит
- [ ] `test_missing_color_field` — отсутствие любого из 22 полей → ошибка
- [ ] `test_invalid_hex` — некорректный HEX → ошибка
- [ ] `test_scan_themes_dir_ignores_subdirs` — вложенные папки игнорируются
- [ ] `test_create_default_themes_no_overwrite` — существующие файлы не перезаписываются
- [ ] `test_theme_field_default` — отсутствие `theme` в settings.toml → "light"

## Критерии готовности
1. Темы загружаются из TOML-файлов (`ThemeData::load_from_file`)
2. Пользователь может добавить свою тему созданием файла в `themes/`
3. Имена `dark` и `light` закреплены за приложением: файлы восстанавливаются
   при отсутствии, но не перезаписываются при наличии
4. Выбор темы сохраняется в `settings.toml` как строка (`theme = "<имя>"`)
5. UI отображает список доступных тем + name/description выбранной
6. Хардкод цветов удалён из `apply_theme()`, цвета берутся из ThemeData

## Заметки
- Дефолт при отсутствии поля `theme` — `"light"`
- Fallback при ошибке загрузки темы — дефолтная Light
  (при недоступности light.toml — из DEFAULT_LIGHT_TOML в коде)
- Логирование ошибок загрузки тем обязательно