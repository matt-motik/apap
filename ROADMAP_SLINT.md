# ROADMAP — миграция GUI на Slint

Решение принято (сессия, cен 2026): переписать слой интерфейса на **Slint**,
оставив ядро без изменений. Причины выбора:
- устали от точечной компоновки egui (item_spacing / separator / пиксель-точность);
- Slint даёт декларативные fix-сетки и детерминированную геометрию;
- нужен спектральный визуализатор (пример `slintfft.example` доказывает декларативную реализацию);
- готовые Material-компоненты.

Ключевые параметры:
- **Рендер:** software-renderer (как в `slintfft.example`), CPU.
- **UI-описание:** отдельные `.slint` файлы (по одному на элемент) + `build.rs` (`slint-build`).
- **Ядро сохраняется как есть:** `audio/` (Player, decoder, output), `playlist.rs`, `meta.rs`, `tray.rs`, `settings.rs`.
- **Старый egui-код (`app.rs`) НЕ удаляется**, пока MVP не подтверждён; чистим потом.
- **Спектр на старте MVP** — симуляция из примера; реальный источник сэмплов — отдельный этап.

---

## Архитектурный сдвиг

Сейчас:
```
main.rs → eframe::run_native → MusicApp: eframe::App (logic/ui)
```
После миграции:
```
main.rs → Slint AppWindow (slint::Window) + Timer(16ms)
                  │
                  ▼
            MusicApp (холдер ядра)
              player / playlist / meta / tray / settings
                  ▲
   callback / in-property (модели, позиция, состояние)
```

`MusicApp` перестаёт быть `eframe::App`. Чистые методы (play_track, add_paths,
сортировка, poll_tray, push_tray_status, автосохранение) переносятся как есть,
без egui-зависимостей; отрисовка целиком уходит в `.slint`.

Связь UI↔Rust:
- `in property <..>` — позиция, время, трек, спектр, состояние repeat/shuffle/volume.
- `callback` — действия: play/pause/stop/prev/next, seek, volume, mute, выбор трека.
- `VecModel`/`ModelRc` — таблица треков и спектральные данные.
- `ui_handle.as_weak()` — обновление из потока/таймера.

---

## Этапы

### S0 — Каркас Slint
- [ ] Cargo.toml: добавить `slint` (features: `software-renderer`), `rustfft`,
      `num-complex`; build-dependency `slint-build`. Убрать eframe/egui из финального
      бинарника (пока держим egui-код, но в отдельном модуле/фиче).
- [ ] `build.rs` компилирует `ui/*.slint` → генерирует Rust-модуль.
- [ ] Новый `main.rs`: создать `AppWindow::new()`, `Timer` 60fps, без eframe.
- [ ] X11-бэкенд (как в egui-версии: принудительный X11 для трей show/hide).

### S1 — Верхняя панель (`ui/top_panel.slint`)
Перенос текущей раскладки (эквивалент `TopLayout`):
- [ ] Col 0: обложка + 6 плоских квадратных кнопок (Play/Pause, Stop, Prev, Next, Repeat, Shuffle).
- [ ] Col 1: инфо (13 строк, вертикальный скролл).
- [ ] Col 2: visualizer (симуляция спектра) + seek-слайдер + время (текущее/общее).
- [ ] Col 3: вертикальная громкость + mute.
- [ ] 3 вертикальных разделителя; `gap`/`cover_size`/`col_info_w` из `settings.rs`.

### S2 — Плейлист (`ui/playlist.slint`)
- [x] Колонки (порядок/видимость/заголовки из настроек) через `StandardTableView` + `VecModel` (`ui/playlist.slint`, `build_rows`/`build_columns`).
- [x] Сортировка по клику на заголовок — `sort-ascending`/`sort-descending` → `sort_rows_compare` (вынесены в `playlist.rs` lib).
- [x] Клик по строке (левый, `PointerEventKind.up`) → `play-track` → `Player::open` + play (как в egui-версии — по одному клику).
- [x] Текущий трек: `current-row` (выделение строки); prev/next ходят по плейлисту (`playlist::advance_index`).

### S3 — Настройки + статус-бар (`ui/settings.slint`, `ui/status.slint`)
- [ ] Окно настроек: тема, cover_size / col_info_w / col_gap слайдеры, audio device,
      колонки (видимость/порядок), repeat/shuffle, minimize_to_tray.
- [ ] Статус-бар: строка состояния сканирования/воспроизведения.

### S4 — Связь с ядром и поведение
- [ ] Callback: транспорт, seek(scrub), volume(слайдер+колесо), repeat/shuffle, mute.
- [ ] Timer обновляет: позиция, время, статус, tray-state, спектр (симуляция).
- [ ] Трей: `tray::start()`, опрос `TrayCmd` в цикле, `push_tray_status`.
- [ ] Перетаскивание файлов, сканирование папки, горячие клавиши.

### S5 — Реальный спектральный визуализатор
- [ ] Ядро отдаёт недавние PCM-сэмплы (перехват в `audio_callback_*`/`output.rs`).
- [ ] FFT (`rustfft`) → сглаживание (Attack & Decay из примера) → `set_spectrum_data`.
- [ ] Настройки режима визуализации (как в P3 egui-родмапа).

### S6 — Чистка и сверка
- [ ] Собрать оба пути? Нет: переключиться на Slint-бинарник, egui-код в архиве/фиче.
- [ ] `cargo build` + `cargo test` (18 тестов ядра `src/audio`, `playlist`, `settings` — зелёные).
- [ ] Визуальная сверка макета с egui-версией.
- [ ] Обновить `FEATURE_COMPARISON.md`, удалить egui-слой (`app.rs`, eframe/egui deps).

---

## Что НЕ трогаем
- Логика ядра: `audio/`, `playlist.rs`, `meta.rs`, `tray.rs`, `settings.rs`.
- Сохранение на диск (`SettingsStore::save`, `save_track_list`) — только вызовы.

## Зависимости (по мере продвижения)
- Сейчас: eframe/egui (временное), egui_extras, Symphonia, cpal, serde/toml, dirs,
  walkdir, ksni, tokio, winit, rand.
- Slint: `slint` (software-renderer) + `slint-build`.
- Спектр (S5): `rustfft`, `num-complex` (уже в примере).

## Структура .slint-файлов (по одному на элемент)
```
ui/
  app.slint        — корневое окно, компоновка верх/центр/низ
  top_panel.slint  — верхняя панель (4 колонки)
  playlist.slint   — таблица треков
  settings.slint   — окно настроек
  status.slint     — статус-бар
```
