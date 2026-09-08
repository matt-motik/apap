# AGENTS.md

Музыкальный плеер на Rust + Slint. Классическое ядро-библиотека (`music_player_rs`) + бинарник (`music-player-rs`) на Slint-интерфейсе, системный трей через StatusNotifier (ksni).

## Команды

```sh
cargo build          # dev-сборка
cargo build --release # релиз (opt-level=3, lto)
cargo run            # запустить плеер
cargo test           # юнит-тесты (пока только в src/cover.rs)
cargo clippy         # линт
```

## Структура исходников

```
build.rs             # компилирует ui/app.slint в Rust (build-dependency: slint-build)
ui/                  # Slint UI (.slint)
  app.slint          # AppWindow — собирает всё вместе, экспортирует колбэки и свойства
  top_panel.slint    # верхняя панель (обложка, трек, управление)
  playlist.slint     # таблица плейлиста
  status.slint       # строка состояния
  settings.slint     # диалог настроек
  theme.slint        # палитра цветов (тёмная/светлая)
  icons/             # SVG-иконки
src/
  lib.rs             # корневая библиотека: audio, cover, meta, playlist, settings, tray
  main.rs            # бинарник — создаёт AppWindow, MusicApp, таймер tick (100 мс), run_event_loop_until_quit
  app/               # MusicApp разбит на модули (God Object → 3 менеджера)
    mod.rs           # struct, new/init/tick, bind_callbacks, трей, helpers, тесты
    ui_manager.rs        # UI-синхронизация, колонки плейлиста, темы (impl MusicApp)
    playback_manager.rs  # воспроизведение/транспорт, shuffle, обложки (impl MusicApp)
    playlist_manager.rs  # трек-лист, сканирование папок, сортировка (impl MusicApp)
  cover.rs           # обложки альбомов: папка/embedded/internet, фоновый воркер, кэш на диск
  meta.rs            # метаданные треков (symphonia probe)
  playlist.rs        # загрузка/сохранение M3U, сканирование папок, Track, сортировка
  settings.rs        # настройки (toml), SettingsStore, ColumnId, RepeatMode, AppConfig
  tray.rs            # трей (ksni): PlayerTray, TrayCmd, TrayState, start()
  audio/             # аудио-подсистема
    mod.rs
    player.rs        # Player (управление воспроизведением)
    decoder.rs       # декодер (symphonia)
    dsd.rs           # DSD-декодер (DSF/DFF, CIC)
    output.rs        # устройство вывода (cpal), выбор устройства, bit-perfect
```

- `app/*` — «Ромб-декомпозиция»: методы `MusicApp` разнесены по `impl`-блокам в подмодулях. Все поля остаются в `mod.rs`, подмодули вызывают методы друг друга через `pub(super)`.

### Как связывается Slint

- `build.rs` компилирует `ui/app.slint` → генерирует структуру `AppWindow` в Rust (`slint::include_modules!()` в `app.rs`).
- `AppWindow` содержит `in-property`, `callback` и т.д.; Rust подписывается через `ui.on_<callback>(...)` и читает/пишет через `ui.set_<prop>(...)` / `ui.get_<prop>()`.
- Slint-файлы импортируют друг друга через `import { Name } from "file.slint";`. `app.slint` — точка входа, остальные — компоненты.
- Стиль UI задан в `build.rs` (`material`).

### Единый экземпляр конфига (AppConfig)

- `MusicApp` — единственный мутирующий владелец `SettingsStore`; читает с диска один раз при старте (`SettingsStore::load()`).
- `AppConfig::init()` публикует read-only снапшот в процессный `OnceLock` для остальных модулей (tray, cover, output) — без повторного чтения диска.
- Снапшот автоматически обновляется в `SettingsStore::save()`, поэтому читатели не видят устаревших значений.

## Features

Собственных Cargo features нет (в Cargo.toml `[features]` отсутствует).

Зависимости подключают свои features:
- `symphonia` — кодеки: `flac, wav, aiff, pcm, mp3, ogg, aac, alac, isomp4, vorbis, adpcm`.
- `slint` — `std, compat-1-2, backend-winit-x11, renderer-winit-software` (X11-бэкенд, программный рендер).
- `tokio` — `rt, macros, sync` (только для `mpsc` в tray).

## Запуск с разными конфигурациями

- Обычный запуск: `cargo run`.
- Трей требует StatusNotifier-хост (KDE/GNOME AppIndicator). Если хоста нет, трей молча не появляется — приложение продолжает работать.
- Эмуляции бэкенда/winit в коде нет. Для headless-тестирования Slint предоставляет тестовый бэкенд, но в проекте он не настроен.

## Тесты

- Модульные тесты есть только в `src/cover.rs` (`percent_encode`, `sniff_ext`, `folder_cover`, `write_cover`, `cover_priority_keys_roundtrip`).
- Каталогов `tests/` (интеграционных) нет.
- `cargo test` запустит только эти тесты.
