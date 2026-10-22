# Архитектура и команды (WF1.0)

Технический справочник текущего состояния репозитория: карта исходников,
команды верификации, тесты, связка Slint ↔ Rust, зависимости, запуск.
Регистрация задачи — `task_intake.md`; микро-декомпозиция и вайтлист —
`planning.md`; цикл шага — `execution.md`; закрытие — `closing.md`. При
противоречии с этим файлом и действующей документацией продукта
(`docs/01_audio_modes_v1.0/`, `docs/02_settings_persistence_v1.0/`) действует
документация — этот файл описывает факт, не фиксирует требование.

## 1. Проект

Музыкальный плеер на Rust + Slint. Библиотека `music_player_rs` (`src/lib.rs`)
+ бинарник `music-player-rs` (`src/main.rs`, Slint UI). Трей через
StatusNotifier (`ksni`, только Linux). `AppCore` (`src/core/mod.rs`) — логика
приложения без Slint (ADR-19, §2.1): настройки, состояние, трек-лист, шлюз
главного окна, центр сообщений; `src/app/` (бинарник) — тонкая прослойка
Slint над `AppCore`. Поток движка `apap-engine` (`src/engine/`, §2.8, §4,
ADR-01) владеет `Player` целиком; UI общается с ним через команды/события,
не напрямую.

## 2. Команды верификации

```sh
cargo build                     # dev-сборка
cargo build --release           # релиз (opt-level=3, lto="fat", codegen-units=1, panic="abort")
cargo run                       # запустить плеер
cargo test                      # lib + bin + tests/*.rs — см. §5 за счётчиками
cargo test <name>                # один тест по фильтру
cargo test audio::               # только тесты аудио-подсистемы (lib)
cargo clippy                     # линт; 0 новых предупреждений в изменённых файлах
python tools/acceptance.py status  # кросс-ОС приёмка спеки — отдельный этап при закрытии спеки, не после каждой задачи
```

Периодичность `cargo test`/`cargo clippy` внутри цикла шага (не на каждом
шаге, но обязательно на ЧЕКПОИНТе и перед закрытием задачи) — `execution.md`,
не здесь.

Сборка **требует** системные пакеты `g++` и `libstdc++-dev` (Skia GPU-рендер
Slint). При `cargo build` компиляция `.slint` печатает warning'и
(padding/width-height) — штатно, не ошибка.

## 3. Карта исходников

```
build.rs             # компилирует ui/app.slint в Rust (build-dependency: slint-build); ошибка UI — код возврата, без паники
hooks/pre-commit      # 5 проверок: state_tool check, check-whitelist, traceability_tool check, acceptance.py check, check_rt_imports
tools/
  state_tool.py        # _STATE_.yaml <-> _STATE_.md (render/check/check-whitelist)
  traceability_tool.py # сверка SPEC <-> ROADMAP (префиксы, ID этапов)
  acceptance.py         # кросс-ОС пакеты приёмки (docs/acceptance/), status/check
  check_rt_imports.py   # запрещённые конструкции в src/audio/render/** (АМ1.0 §7.7)
  gen_test_audio.py     # генерация эталонных аудиофайлов для tests/data (АМ1.0 §7.1)
  setup_dev.py          # установка git hooksPath и т.п.
  wheel_probe/          # отдельный Cargo-крейт: сбор сырых событий колёсика по ОС (README в папке)
ui/                   # Slint UI (.slint)
  app.slint            # AppWindow — собирает всё вместе, экспортирует колбэки/свойства
  top_panel.slint      # верхняя панель (обложка, трек, управление)
  playlist.slint        # таблица плейлиста
  tableview_fork.slint  # форк Slint TableView (сортировка/клики)
  fork_components.slint # вспомогательные компоненты для таблицы
  status.slint          # строка состояния
  settings.slint        # диалог настроек
  mode_params.slint     # параметры режима вывода (АМ1.0)
  message_window.slint  # модальные сообщения/диалоги
  visualizer.slint      # виджеты визуализации (спектр/осциллограмма/спектрограмма)
  theme.slint            # палитра цветов (тёмная/светлая)
  icons/, icons/dark/, icons/light/  # SVG-иконки (светлая/тёмная тема отдельно от общих)
src/
  lib.rs               # корень библиотеки: audio, core, cover, engine, journal, meta, persist, platform, playlist, playlist_layout, settings, theme
  main.rs              # бинарник: создаёт AppWindow/MusicApp, таймеры (tick 100мс, column-reflow 16мс, спектр ~30fps), run_event_loop
  app/                 # MusicApp (бинарник, тонкий Slint-слой над AppCore), ~4000 строк в mod.rs
    mod.rs, events.rs    # struct MusicApp (new/init/tick/bind_callbacks) + направленный event-feed AppEvent между менеджерами
    ui_manager.rs, playback_manager.rs, playlist_manager.rs  # UI/колонки/темы; транспорт/shuffle/обложки; трек-лист/сканирование/сортировка
    visualizer_manager.rs, fulltrack_manager.rs, viz_settings_manager.rs  # live-спектр (воркер+tap); осциллограмма/спектрограмма; настройки визуализации в диалоге
    badge_view.rs, lock_view.rs, mode_dialog.rs, path_view.rs  # вью режима: бейдж, блокировка элементов, диалог выбора, отчёт PathReport
    ui_audio_state.rs, engine_sink.rs  # агрегированное аудио-состояние UI; приём событий потока движка
  core/                # AppCore — логика без Slint (ADR-19, §2.1)
    mod.rs               # struct AppCore, настройки/состояние/трек-лист
    dialog.rs, exit.rs, gate.rs, geometry.rs, io.rs, load.rs, messages.rs, volume.rs, testing.rs
  engine/              # поток apap-engine (§2.8, §4, ADR-01); Player живёт целиком внутри
    mod.rs, deps.rs, exclusive_path.rs, legacy_path.rs, lock.rs, messages.rs, run.rs, sink.rs, source.rs, spawner.rs
    tests.rs             # #[cfg(test)]
  persist/             # хранение настроек/состояния/плейлиста (docs/02_settings_persistence_v1.0/)
    mod.rs, keys.rs, settings_file.rs, state_file.rs, tracker.rs, writer.rs
  platform/            # трейты без cfg + реализации по ОС + фейки для тестов (ADR-6, ТЗ-54)
    mod.rs, devwatch.rs
    fs/                  # linux.rs, macos.rs, windows.rs, mem.rs (фейк), engine_store.rs
    input/               # обработка колёсика: macos.rs, windows.rs, winit.rs, matcher.rs, normalizer.rs, table.rs, fake.rs, fixture_tests.rs, wheel_table.toml
    lifecycle/            # macos.rs, unix.rs, windows.rs, fake.rs
    notify/               # linux.rs, none.rs, fake.rs
    pick/                 # rfd.rs (нативный диалог), fake.rs
    tray/mod.rs           # StatusNotifier-трей (замена прежнего src/tray.rs)
  playlist/            # Track, M3U, сканирование (заменил бывший src/playlist.rs)
    mod.rs, compare.rs, load.rs, model.rs, shuffle.rs
  settings/            # модуль-файл src/settings.rs + подмодули params.rs, playback.rs, playback_dto.rs
  audio/               # аудио-подсистема
    mod.rs
    backend/             # устройства/бэкенды вывода (АМ1.0 §2.3): alsa.rs, alsa_out.rs, busy_owner.rs, catalog.rs, exclusive.rs, os_probe.rs, shared.rs
    path/                # модель пути сигнала (АМ1.0 §3.1-3.3): plan.rs, recommend.rs, report.rs, signal.rs, transform.rs, plan_tests.rs
    render/              # RT-колбэк вывода (§6.14-6.17): mod.rs, dop.rs, gain.rs, out.rs, pcm.rs, tpdf.rs — 0 аллокаций/блокировок/логов, проверяет tools/check_rt_imports.py
    reservation/         # ReserveDevice1 по D-Bus (ADR-08, §6.6): mod.rs (trait ReserveBus, acquire), dbus.rs (zbus)
    testing/             # тестовая инфраструктура тракта (АМ1.0 §7.1, ADR-20): mod.rs, signals.rs — компилируется всегда
    decoder.rs, dsd.rs    # декодер (symphonia), DSD-декодер (DSF/DFF, CIC)
    output.rs, worker.rs  # устройство вывода (cpal), воркер аудио-потока
    player.rs             # Player (управление воспроизведением, живёт в src/engine/)
    clock.rs, dop.rs, error.rs, format.rs, session.rs, session_build.rs, test_run.rs, verdict.rs
    analyzer.rs           # live-анализатор спектра (SpectrumEngine)
    spectrogram.rs, fulltrack.rs, palettes.rs, visualizer.rs
    rt.rs                 # #[cfg(target_os = "linux")]
  cover.rs, meta.rs, playlist_layout.rs, journal.rs, theme.rs
tests/                 # интеграционные тесты (каталог СУЩЕСТВУЕТ)
  dsd512_playback_cpu_budget.rs, fulltrack_memory_budget.rs, live_worker_zero_alloc.rs,
  parsers_survive_mutations.rs, perf_budgets.rs, rt_zero_alloc.rs
  data/                # эталонные FLAC/MP3/DSF/DFF (+ мутации), data/wheel/*.fixture (кросс-ОС фикстуры колёсика)
```

`app/*` — «Ромб-декомпозиция»: методы `MusicApp` разнесены по `impl`-блокам в
подмодулях, поля — в `mod.rs`, вызовы между подмодулями через `pub(super)`.
`src/settings.rs`/`src/playlist.rs` рядом с папками `src/settings/`,
`src/playlist/` — не дублирование: стандартный модуль-файл Rust 2018+ (корень
+ подмодули в одноимённой папке); `playlist.rs` — переименованный из плоского
файла в такой каталог.

### 3.1. Каталог → спецификация (для ссылок `ТЗ-N`/`ADR-N`/`§X.Y`)

Номера `ТЗ-N`, `ADR-N`, `§X.Y` у AM1.0 и SP1.0 пересекаются (`ТЗ-8` — дизеринг
в AM1.0 и разбор ключей в SP1.0). Ссылка **без префикса** относится к спеке
модуля по таблице ниже; ссылка на **другую** спеку и любая ссылка в
смешанном файле пишется с префиксом: `SP1.0 ТЗ-8`, `AM1.0 §2.2`. Встречающаяся
в коде запись `02 §2.4` — прежняя форма того же префикса (01 = AM1.0,
02 = SP1.0); в новом коде — `AM1.0`/`SP1.0`. Старые ссылки массово не
переписываются, префикс добавляется при правке строки.

| Спека модуля | Каталоги и файлы |
|---|---|
| AM1.0 (`docs/01_audio_modes_v1.0/`) | `src/audio/**`, `src/engine/**`, `src/platform/devwatch.rs`, `src/settings/params.rs`, `src/settings/playback.rs`, `src/app/{badge_view,lock_view,mode_dialog,path_view,ui_audio_state,engine_sink}.rs`, `ui/mode_params.slint`, `tests/{rt_zero_alloc,dsd512_playback_cpu_budget,live_worker_zero_alloc}.rs` |
| SP1.0 (`docs/02_settings_persistence_v1.0/`) | `src/core/**`, `src/persist/**`, `src/platform/**` (кроме `devwatch.rs`), `src/journal.rs`, `ui/message_window.slint` |
| T1.0 / V5.1 | `src/theme.rs` (T1.0); визуализация `src/audio/{analyzer,spectrogram,fulltrack,palettes,visualizer}.rs`, `src/app/{visualizer,fulltrack,viz_settings}_manager.rs` (V5.1 — исключение из строки AM1.0) |
| смешанные — только с префиксом | `src/app/mod.rs` и прочие `src/app/*_manager.rs`, `src/main.rs`, `src/lib.rs`, `src/settings.rs`, `src/settings/playback_dto.rs`, `src/playlist/**`, `ui/*.slint` (кроме указанных), `tests/*.rs` (кроме указанных) |

Новый файл добавляется в таблицу тем же коммитом, что и сам файл.

## 4. Связка Slint ↔ Rust

- `build.rs` компилирует `ui/app.slint` → структура `AppWindow` в Rust
  (`slint::include_modules!()` в `src/main.rs`/`src/app/mod.rs`).
- `AppWindow` содержит `in-property`/`callback`; Rust подписывается через
  `ui.on_<callback>(...)`, читает/пишет через `ui.set_<prop>(...)` /
  `ui.get_<prop>()`.
- `.slint`-файлы импортируют друг друга через `import { Name } from "file.slint";`;
  `app.slint` — точка входа, остальные — компоненты.
- Стиль UI — `fluent`, задан в `build.rs`.

## 5. Тесты

```sh
cargo test 2>&1 | grep -E "^test result|Running"   # точные счётчики на конкретный коммит
```

Счётчики на момент написания (не обновлять вручную при каждом изменении —
перезапускать команду выше): lib (`src/**`) — 1092 passed, 2 ignored; bin
(`src/app/**`) — 110 passed; интеграционные `tests/*.rs` — по одному бинарнику
на файл, большинство 0-1 тестов (`perf_budgets.rs` — 4), один `#[ignore]`
(`dsd512_playback_cpu_budget.rs`); doctests — 0.

- `#[ignore]`-тесты (требуют реального оборудования, не гоняются в CI):
  `src/audio/backend/alsa.rs`, `src/audio/reservation/dbus.rs`,
  `tests/dsd512_playback_cpu_budget.rs`. Запуск ALSA-теста на реальной карте:
  `APAP_HW_TEST_DEVICE=hw:CARD=...,DEV=0 cargo test --lib -- --ignored` (точный
  вызов — doc-комментарий у теста в `alsa.rs`).
- `src/audio/testing/` и `tests/data/` — общая инфраструктура: синтетические
  сигналы, эталонные FLAC/MP3/DSF/DFF (+ файлы с мутациями заголовков для
  `parsers_survive_mutations.rs`), кросс-ОС фикстуры колёсика
  (`tests/data/wheel/{gnome,kde,windows}-N/*.fixture`, генерируются
  `tools/wheel_probe/`).
- Каталогов `tests/` «нет» — неверно, см. список в §3; это интеграционные
  тесты сверх lib/bin-юнитов.

## 6. Зависимости и features

Собственных Cargo features нет. Версии и features из `Cargo.toml`:

- `slint` 1.17, `default-features = false` + `std, compat-1-2,
  backend-winit-x11, backend-winit-wayland, renderer-winit-skia-opengl,
  raw-window-handle-06, unstable-winit-030` (X11 и нативный Wayland, GPU
  через OpenGL); build-dependency `slint-build` 1.17.
- `symphonia` 0.6 — кодеки `flac, wav, aiff, pcm, mp3, ogg, aac, alac,
  isomp4, vorbis, adpcm`.
- `tokio` 1.53.1 — `rt, macros, sync, signal` (везде); на Linux дополнительно
  `rt-multi-thread` (служебный поток D-Bus резервирования под `zbus`/`ksni`).
- Аудио/визуализация: `cpal` 0.18, `rustfft` 6.4.1, `rtrb` 0.4.0, `clru` 0.6,
  `smallvec` 1.16.
- Данные/интеграции: `rfd` 0.17, `ureq` 2 + `tls`, `serde`/`serde_json`/
  `toml` 1, `dirs` 6, `walkdir` 2, `rand` 0.8, `image` 0.25 (`png`),
  `icu_normalizer` 2.3 (NFC-нормализация ключей сравнения плейлиста, ОВС-3а).
- По целевой ОС: Unix — `libc` 0.2 (`O_DIRECTORY`, `F_FULLFSYNC`); Linux —
  `ksni` 0.3.6 (трей), `zbus` 5.19 (ReserveDevice1), `alsa` 0.11 (собственный
  Exclusive-вывод), `rustix` 1.1 (`process, thread`); macOS — `objc2` 0.5.2,
  `block2` 0.5.1, `objc2-app-kit` 0.2.2 (`NSEvent, block2`); Windows —
  `windows-sys` 0.61 (`Win32_Foundation, Win32_Storage_FileSystem,
  Win32_System_Registry, Win32_UI_Shell, Win32_UI_WindowsAndMessaging`).
- `[lints.clippy]`: `unwrap_used = "deny"`, `expect_used = "deny"`,
  `unreachable = "deny"` (прод-код; тесты — исключение через `clippy.toml`).

## 7. Запуск и окружение

- Обычный запуск: `cargo run`.
- Трей требует StatusNotifier-хост (KDE/GNOME AppIndicator). Без хоста трей
  молча не появляется — приложение продолжает работать.
- Headless-бэкенд Slint в проекте не настроен.
- На Linux-десктопе `cp` в `zsh` интерактивен по алиасу — `cp` без `-f`
  зависает на подтверждении перезаписи; использовать `command cp -f` или
  `shutil.copy` вместо голого `cp`.
