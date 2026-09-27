# wheel_probe

Эксперимент задачи `docs/02_settings_persistence_v1.0/` (решение 3, ТЗ-141 и ОВС-20 в `docs/01_audio_modes_v1.0/`). Программа записывает **сырые** дельты колёсика мыши и тачпада:

- над ползунком громкости — копия `ui/top_panel.slint:464-492` плеера (`TouchArea` со `scroll-event` вокруг вертикального `Slider`);
- над иконкой трея — `ksni` (StatusNotifierItem), как `src/tray.rs` плеера. На Windows и macOS у плеера трея нет, поэтому шаги трея там не предлагаются.

Значения не нормализуются и не округляются. Анализ — в `docs/02_settings_persistence_v1.0/wheel_test.md`.

## Запуск

```sh
cd tools/wheel_probe
cargo run --release
```

Окно ведёт по шагам: на каждом шаге указаны устройство, цель (ползунок или трей) и действие. Перед нажатием «Готово» проверьте поля «Устройство» и «Естественная прокрутка».

- «Повторить» — начать шаг заново (записанные события шага сбрасываются).
- «Пропустить» — пропустить шаг.
- «Пропустить устройство» — пропустить все шаги устройства, например если высокоточной мыши нет.

Закрыть окно можно в любой момент: уже выполненные шаги сохранены.

**KDE: X11 и Wayland.** Лучше замерять в двух настоящих сессиях Plasma: войти в «Plasma (X11)», затем в «Plasma (Wayland)». Запуск в Wayland-сессии через XWayland (`WAYLAND_DISPLAY= cargo run --release`) даёт бэкенд окна `x11`, но события проходят через XWayland. Это видно в `env.txt` как `XDG_SESSION_TYPE: wayland` вместе с `window_backend: x11`.

## Результаты

Каталог `results/<os>_<backend>_<desktop>/`, например `results/linux_wayland_kde/`. Если каталог уже есть, к имени добавляется `-2`, `-3` и так далее.

- `env.txt` — ОС, рабочий стол, фактический бэкенд окна (`x11`, `wayland`, `win32` или `appkit` по raw window handle), версии крейтов, статус трея, описание колонок.
- `NN_<device>_<target>_<action>.csv` — по одному файлу на выполненный шаг, колонки `t_ms,source,delta_x,delta_y,orientation,modifiers`:
  - `t_ms` — время от начала шага;
  - `source=slint` — `scroll-event` ползунка, delta в логических пикселях;
  - `source=winit` — `WindowEvent::MouseWheel` окна; `orientation` = `line` или `pixel` плюс `/`TouchPhase; delta — сырые значения winit;
  - `source=tray` — `ksni::Tray::scroll`: `delta_y` = `delta`, `orientation` = `Vertical` или `Horizontal`.
- `summary.txt` — по каждому шагу и источнику: число событий, уникальные значения с числом повторов, min и max, сумма и её знак, медиана интервалов; список пропущенных шагов.

Результаты коммитятся в git (`target/` — нет).

## Версии

Версии крейтов совпадают с корневым `Cargo.lock` плеера: `slint` 1.17.1, `winit` 0.30.13, `ksni` 0.3.6, `tokio` 1.53.1, `zbus` 5.19.0. Исходный `Cargo.lock` скопирован из корня.

От плеера отличается одно: рендерер программный (`renderer-software`) вместо Skia. На события ввода это не влияет, зато крейт собирается без Skia на любой ОС. Стиль виджетов — `fluent`, как у плеера (`.cargo/config.toml`).

## Проверка сборки

```sh
cargo build --release
cargo clippy --release --all-targets -- -D warnings
cargo test --release
```

Для замера на другой ОС через агента opencode — `OPENCODE.md`.
