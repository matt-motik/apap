# AGENTS.md

Музыкальный плеер на Rust + Slint. Классическое ядро-библиотека + бинарник на Slint-интерфейсе, системный трей через StatusNotifier (ksni).

## Команды

```sh
cargo build          # dev-сборка
cargo build --release # релиз (opt-level=3, lto)
cargo run            # запустить плеер
cargo test           # запустить юнит-тесты
cargo clippy         # линт
```

Тестов пока нет (каталоги `**/tests` и `#[cfg(test)]` отсутствуют).

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
  lib.rs             # корневая библиотека: audio, meta, playlist, settings, tray
  main.rs            # бинарник — создаёт AppWindow, MusicApp, таймер tick (100 мс), run_event_loop_until_quit
  app.rs             # MusicApp: логика приложения, связывание колбэков, трей, tick
  meta.rs            # метаданные треков
  playlist.rs        # загрузка/сохранение M3U, сканирование папок
  settings.rs        # настройки (toml), репозиторий SettingsStore
  tray.rs            # трей (ksni): PlayerTray, TrayCmd, TrayState, start()
  audio/             # аудио-подсистема
    player.rs        # Player (управление воспроизведением)
    decoder.rs       # декодер (symphonia)
    dsd.rs           # DSD-декодер
    output.rs        # устройство вывода (cpal)
    mod.rs
```

### Как связывается Slint

- `build.rs` компилирует `ui/app.slint` → генерирует структуру `AppWindow` в Rust (`slint::include_modules!()` в `app.rs`).
- `AppWindow` содержит `in-property`, `callback` и т.д.; Rust подписывается через `ui.on_<callback>(...)` и читает/пишет через `ui.set_<prop>(...)` / `ui.get_<prop>()`.
- Slint-файлы импортируют друг друга через `import { Name } from "file.slint";`. `app.slint` — точка входа, остальные — компоненты.
- Стиль UI задан в `build.rs` (`material`).

## Features

Собственных Cargo features нет (в Cargo.toml `[features]` отсутствует).

Зависимости подключают свои features:
- `symphonia` — кодеки: `flac, wav, aiff, pcm, mp3, ogg, aac, alac, isomp4, vorbis, adpcm`.
- `slint` — `std, compat-1-2, backend-winit-x11, renderer-winit-software` (X11-бэкенд, программный рендер).

## Запуск с разными конфигурациями

- Обычный запуск: `cargo run`.
- Трей требует StatusNotifier-хост (KDE/GNOME AppIndicator). Если хоста нет, трей молча не появляется — приложение продолжает работать.
- Эмуляции бэкенда/winit в коде нет. Для headless-тестирования Slint предоставляет тестовый бэкенд, но в проекте он не настроен.
