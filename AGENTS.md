# AGENTS.md

> Перед началом: прочитать `_STATE_.md` — точка входа сессии.
> Если незавершённый шаг → продолжение; если done/отсутствует → новая задача.

Воркфлоу построен вокруг трёх артефактов:
- `_STATE_.md` — состояние текущей сессии (точка входа при продолжении на другой машине, в новой сессии или другим агентом).
- `_TODO_/` — инбокс: пользователь кладёт сюда замечания/задачи от внешних ИИ; агент превращает их в пункты `ROADMAP.md`.
- `ROADMAP.md` — единый долгосрочный план проекта (единственный постоянный .md-план в корне).

Шаг 0: Инициализация и состояние

    Проверить .agent-session.lock. Если существует и процесс мертв → предупредить о возможном краше предыдущей сессии.
    Прочитать _STATE_.md:
        Если там незавершённый шаг → Режим "Продолжение" (восстановление из _STATE_.md + git status/diff).
        Если состояние done/отсутствует → Режим "Новая задача" (завести новый _STATE_.md).

Шаг 1: Аудит Репозитория (Безопасность)

    git status --porcelain.
    Если грязно:
        Запустить git diff --stat для отчета.
        "Новая задача": Предложить стратегии (WIP Commit / Stash / Reset).
        "Продолжение": Сверить изменённые файлы с разделом "Изменяемые файлы" в _STATE_.md. Если не совпадают → Стоп, запрос ручной резолюции.
    Проверка компиляции: cargo check --quiet. Если ошибки → Стоп. Сообщить, что код в нерабочем состоянии.

Шаг 2: Обработка инбокса _TODO_

    Отсканировать _TODO_/ (кроме подкаталога done/).
    Каждый файл — замечание/задача от внешнего ИИ:
        дедупликация по ROADMAP.md (если уже есть — пропустить);
        если новая — добавить в ROADMAP.md с этапом и приоритетом.
    Перенести отработанные файлы в _TODO_/done/.
    Обновить _STATE_.md (если инбокс дал активную задачу).

Шаг 3: Планирование

    Взять активную задачу (из ROADMAP.md или инбокса), разбить на шаги.
    Записать в _STATE_.md: активная задача, шаги [], current_step, следующий ход, изменяемые файлы, риск/стоп-условие.
    Не плодить plan-*.md и .current-plan.json — вся сессионная информация живёт только в _STATE_.md.

Шаг 4: Цикл Выполнения

    Выполнить один шаг.
    Верификация: cargo build / cargo clippy / cargo test.
        Успех → обновить _STATE_.md (шаг [x], новый "следующий ход") и закоммитить (код + _STATE_.md в одном коммите).
        Провал → записать описание в _STATE_.md, остановиться и ждать инструкций.
    Анти-dead-loop: если после нескольких итераций нет изменений в рабочем дереве/коммитов → прекратить и спросить пользователя.

Шаг 5: Завершение

    Если все шаги [x] и тесты зелены:
        Пометить задачу в ROADMAP.md (например "сделано в <commit>").
        Привести _STATE_.md к состоянию done.
        Показать итоговый git diff --stat.
    Если прервано (токены, сеть, смена машины): состояние в _STATE_.md + git достаточно для продолжения любым агентом.



Музыкальный плеер на Rust + Slint. Классическое ядро-библиотека (`music_player_rs`) + бинарник (`music-player-rs`) на Slint-интерфейсе, системный трей через StatusNotifier (ksni).

## Команды

```sh
cargo build          # dev-сборка
cargo build --release # релиз (opt-level=3, lto)
cargo run            # запустить плеер
cargo test           # юнит-тесты: 23 lib (cover, settings, playlist, audio/dsd, audio/decoder) + 6 bin (app/mod.rs)
cargo test <name>    # один тест по фильтру
cargo clippy         # линт
```

Сборка **требует** системные пакеты: `g++` и `libstdc++-dev` (Skia GPU-рендер Slint).
При `cargo build` компиляция `.slint` печатает warning'и (padding/width-height) — это штатно, не ошибки.

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

- `build.rs` компилирует `ui/app.slint` → генерирует структуру `AppWindow` в Rust (`slint::include_modules!()` в `src/app/mod.rs`).
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
- `slint` — `std, compat-1-2, backend-winit-x11, renderer-winit-skia` (X11-бэкенд, Skia GPU-рендер; требует g++ и libstdc++-dev в системе).
- `tokio` — `rt, macros, sync` (только для `mpsc` в tray).

## Запуск с разными конфигурациями

- Обычный запуск: `cargo run`.
- Трей требует StatusNotifier-хост (KDE/GNOME AppIndicator). Если хоста нет, трей молча не появляется — приложение продолжает работать.
- Эмуляции бэкенда/winit в коде нет. Для headless-тестирования Slint предоставляет тестовый бэкенд, но в проекте он не настроен.

## Тесты

- Модульные тесты: `src/cover.rs` (`percent_encode`, `sniff_ext`, `folder_cover`, `write_cover`, `cover_priority_keys_roundtrip`), `src/settings.rs`, `src/playlist.rs`, `src/audio/dsd.rs`, `src/audio/decoder.rs`; bin-тесты — в `src/app/mod.rs`.
- Каталогов `tests/` (интеграционных) нет.
- `cargo test`: 23 lib + 6 bin.
