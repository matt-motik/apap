# ROADMAP — план доработок и улучшений Music Player на Rust

> Единый долгосрочный план проекта (единственный постоянный .md в корне).
> Он заменяет прежние `docs/task*.md`, `ROADMAP_SLINT.md`, `src/top_panel.md`,
> а также частично `FEATURE_COMPARISON.md` (архив — в git-истории).

**Навигация для агента:**
- `ROADMAP.md` — что вообще надо сделать (этот файл), задачи привязаны к этапам и приоритетам.
- `_STATE_.md` — **точка входа** при продолжении: текущая активная задача, шаг, «следующий ход». Читать первым в новой сессии.
- `_TODO_/` — инбокс: пользователь кладёт сюда замечания/задачи от внешних ИИ; агент превращает их в пункты этого плана и переносит в `_TODO_/done/`.
- Задаче в плане, которая сейчас выполняется, ставится маркер состояния; детали текущего шага живут в `_STATE_.md`.

Этот план составлен с учётом правильного порядка выполнения: каждая следующая задача не требует глобальной переделки предыдущей. Задачи сгруппированы по приоритетам и логическим этапам.

---

## Этап 1: Критические исправления (стабильность, предотвращение паник)

### 1.1. Замена `unwrap()` на безопасную обработку ошибок в `player.rs`

**Статус:** ✅ сделано — все `self.core.lock().unwrap()` (строки 180/204/224/248/264/327) заменены на `match`/`let-else` с graceful-обработкой (см. `src/audio/player.rs`). Паник в UI-потоке при отравленном мьютексе больше нет.

**Файл:** `src/audio/player.rs`  
**Проблема:** Множественные вызовы `lock().unwrap()` могут вызвать панику в UI-потоке, если мьютекс был отравлен в другом потоке.  
**Строки:** 180, 204, 224, 248, 264, 327

**Задача:**
```rust
// Было:
let mut core = self.core.lock().unwrap();

// Стало:
let mut core = match self.core.lock() {
    Ok(guard) => guard,
    Err(_) => return, // или Err(e.to_string())
};
```

**Почему первым:** Это базовое исправление, которое влияет на все методы `Player`. Последующие рефакторинги могут изменить сигнатуры методов, поэтому сначала нужно сделать их безопасными.

---

### 1.2. Обработка ошибок канала в `cover.rs` и `playlist.rs`

**Статус:** ✅ сделано — `cover.rs`: `start_worker` теперь сначала `recv()`, затем подбирает самый свежий джоб через `while let Ok(newer) = rx.try_recv()` (устранена гонка между очисткой и основным `recv()`). `playlist.rs`: `probe_paths` при `send(...).is_err()` (разрыв канала) останавливает сканирование. Примечание: `Sender::is_disconnected()` — unstable в rustc 1.98, поэтому реализовано через проверку результата `send`.

**Файлы:** 
- `src/cover.rs` (строки 78-79)
- `src/playlist.rs` (строка 67)

**Проблема:** 
- В `cover.rs`: гонка между очисткой очереди (`try_recv()`) и основным `recv()` — может пропустить запросы при быстром переключении треков.
- В `playlist.rs`: игнорирование ошибок канала (`let _ = tx.send(...)`) при сканировании.

**Задача:**
```rust
// cover.rs: убрать агрессивную очистку очереди
pub fn start_worker(rx: mpsc::Receiver<CoverJob>, done: mpsc::Sender<CoverDone>) {
    std::thread::spawn(move || loop {
        let job = match rx.recv() {
            Ok(j) => j,
            Err(_) => return,
        };
        // Перед обработкой проверяем, не пришёл ли новый запрос
        while let Ok(newer_job) = rx.try_recv() {
            job = newer_job;
        }
        let image = resolve_cover(&job.track, &job.cfg);
        let _ = done.send(CoverDone { id: job.id, image });
    });
}
```

```rust
// playlist.rs: проверять is_disconnected перед отправкой
if !tx.is_disconnected() {
    let _ = tx.send(ScanMsg::Batch(std::mem::take(&mut batch)));
}
```

---

### 1.3. Неблокирующая проверка устройства в `output.rs`

**Статус:** ✅ сделано — `build_stream` принимает `Option<Arc<AtomicBool>>`; error-callback выставляет флаг при сбое ALSA/PipeWire. `probe_output` вместо блокирующего `sleep(PROBE_OPEN_MS)` поллит флаг через `yield_now()` с дедлайном (не блокирует UI-поток). `player.rs` передаёт `None`.

**Файл:** `src/audio/output.rs`  
**Проблема:** Блокирующий `sleep(PROBE_OPEN_MS)` при probe устройства (строка 294).  
**Решение:** Использовать неблокирующую проверку состояния потока или асинхронный таймаут.

**Задача:**
```rust
// Заменить sleep на проверку флага готовности
let deadline = std::time::Instant::now() + std::time::Duration::from_millis(PROBE_OPEN_MS);
while !error_flag.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
    std::thread::yield_now();
}
```

---

## Этап 2: Архитектурные улучшения (SOLID, разделение ответственности)

### 2.1. Разделение `MusicApp` на отдельные менеджеры

**Статус:** ✅ сделано (основная часть) — декомпозиция `app.rs` → `app/{mod,ui_manager,playback_manager,playlist_manager}.rs` (коммит `8318b6f`). Не выделены как отдельные файлы: `tray_service.rs` и `cover_service.rs` (логика tray и cover живёт в `mod.rs`/`playback_manager.rs`).

**Файл:** `src/app/mod.rs` (961 строка)  
**Проблема:** Класс `MusicApp` нарушает **Single Responsibility Principle** — управляет UI, плеером, плейлистом, настройками, tray, обложками.

**Задача:** Создать явные зависимости между компонентами:

```
MusicApp (координатор)
├── PlaybackManager (уже есть: playback_manager.rs)
├── PlaylistManager (уже есть: playlist_manager.rs)
├── UIManager (уже есть: ui_manager.rs)
├── SettingsStore (уже есть: settings.rs)
├── CoverService (новый сервис)
└── TrayService (вынести из mod.rs)
```

**План рефакторинга:**
1. Вынести логику tray в отдельный модуль `app/tray_service.rs`
2. Создать `app/cover_service.rs` для управления cover_tx/cover_rx
3. Уменьшить `MusicApp` до ~200 строк (только координация и tick())

**Почему вторым этапом:** Сначала нужно исправить критические ошибки (Этап 1), чтобы не переносить баги в новую архитектуру.

---

### 2.2. Dependency Injection вместо глобального `CONFIG`

**Статус:** ✅ сделано — глобальный `CONFIG`/`AppConfig` **полностью удалён** (коммит `350a47a`): у всех модулей уже был явный DI (`CoverConfig::from_settings(&Settings)`, `preferred_name`, `self.settings.settings`), а `AppConfig::settings()`/`is_initialized()` оказались мёртвым кодом — снапшот был write-only. Убраны `static CONFIG`, `AppConfig::init/update`, вызов в `MusicApp::new` и `AppConfig::update` из `SettingsStore::save()`.

**Файл:** `src/settings.rs`  
**Проблема:** Глобальное состояние `AppConfig::init()` усложняет тестирование и нарушает **Dependency Inversion Principle**.

**Задача:**
```rust
// Было (глобальный write-only снапшот): CONFIG: OnceLock<Mutex<Settings>>
AppConfig::init(settings.settings.clone());

// Стало: глобального состояния нет вообще; Settings передаются по ссылке
// (&Settings → CoverConfig::from_settings, MusicApp::settings_ref(), ...)
```

---

### 2.3. Абстракция над `cpal` через trait `AudioHost`

**Статус:** ✅ сделано — коммит `72607fd`. Введены `DeviceInfo`/`RateRange`/`ChosenOutput` (бэкенд-агностичные снимки возможностей устройства), `trait AudioHost` (`devices()`, `default_name()`) с реализацией `CpalHost`; выделена чистая функция `choose_output(...)`; `select_output` делегирует в `choose_output` + `CpalHost::device_by_name()` (контракт `OutputSpec` не изменился). `output_devices()`/`default_device_name()` используют `CpalHost`. Добавлены 9 unit-тестов с `MockHost` (выбор по предпочтению/дефолту, матчинг rate/channels, fallback). Новых clippy-warning'ов нет, 38 тестов зелёные.

**Файл:** `src/audio/output.rs`  
**Проблема:** Прямая зависимость от `cpal::default_host()` затрудняет тестирование и портирование.

**Задача:**
```rust
pub trait AudioHost {
    fn devices(&self) -> Vec<DeviceInfo>;
    fn default_name(&self) -> Option<String>;
}

pub struct CpalHost;
impl AudioHost for CpalHost { ... }

// В тестах использовать MockHost
```

---

## Этап 3: Производительность и оптимизации

### 3.1. Оптимизация сортировки плейлиста без клонирования

**Статус:** ✅ сделано — индексная сортировка `idx.sort_by(... sort_rows_compare(&self.tracks[a], ...))` в `src/app/playlist_manager.rs:160`.

**Файл:** `src/playlist.rs`  
**Проблема:** Сортировка создаёт клоны всех треков.

**Задача:**
```rust
// Вместо:
tracks.sort_by(|a, b| sort_rows_compare(a, b, col));

// Использовать индексную сортировку:
let mut indices: Vec<usize> = (0..tracks.len()).collect();
indices.sort_by(|&i, &j| sort_rows_compare(&tracks[i], &tracks[j], col));
// Применить перестановку к UI без клонирования данных
```

---

### 3.2. Хэш-субдиректории для кэша обложек

**Статус:** ✅ сделано — коммит `628aa2d`. Запись идёт в `dir/<hex[0..2]>/<hex>.<ext>` (поддиректория создаётся при записи), поиск `find_cached` сначала проверяет поддиректорию (≤1 файл, т.е. O(1)), при промахе — fallback на корень кэша (старые flat-кэши продолжают читаться). +3 теста.

**Файл:** `src/cover.rs`  
**Проблема:** Линейный поиск O(n) в `find_cached()` (строка 240-248).

**Задача:**
```rust
// Использовать первые 2 символа хэша как поддиректорию:
fn cache_subdir(dir: &Path, hash: u64) -> PathBuf {
    let hex = format!("{hash:016x}");
    dir.join(&hex[0..2])
}
```

---

### 3.3. Асинхронная загрузка плейлиста при старте

**Статус:** ✅ сделано — коммит `6ecebab`. `MusicApp::new` больше не блокируется на чтении `playlist.m3u`: загрузка уходит в фоновый поток (`channel<Vec<Track>>`), а `tick()` через `drain_startup_tracks()` применяет треки по прибытии — пересобирает shuffle-порядок, заново применяет сохранённую сортировку, синкает UI и показывает «Loaded N tracks». То самое чтение было `src/app/mod.rs:141`; async-загрузка теперь единственный путь и для старта, и для Add Files/Folder.

**Файл:** `src/app/mod.rs` (строки 138-139)  
**Проблема:** Синхронная загрузка плейлиста блокирует инициализацию приложения.

**Задача:**
```rust
// Запускать в фоне при старте:
let (startup_tx, startup_tracks_rx) = channel::<Vec<Track>>();
let startup_path = playlist_path();
thread::spawn(move || {
    let tracks = playlist::load_track_list(&startup_path);
    let _ = startup_tx.send(tracks);
});

// В tick() проверять результат
fn drain_startup_tracks(&mut self) {
    // apply tracks: rebuild_shuffle + apply_sort + sync_playlist_to_ui
}
```

---

## Этап 4: Улучшение архитектуры и тестируемости

### 4.1. Event-driven архитектура для связи компонентов

**Статус:** ✅ сделано (коммит <этап-шаг>) — вместо шины/бродкаста внедрён **направленный event-feed** (`src/app/events.rs`): `enum AppEvent` + односторонний mpsc-канал (`events_tx`/`events_rx`), менеджеры эмитят (`MusicApp::emit`), `tick()` дренирует (`drain_events`) до UI-синка. Поверх поключена **дельта-синхронизация** UI: `last_ui: UiState` хранит последние запушенные значения, `sync_playback_state_to_ui` пишет только реально изменившиеся свойства (playing/muted/volume/pos/dur/seek_fraction/status) — на паузе/стопе сикбар и статус не дёргаются каждый 100 мс тик.

**Что эмитится (события).**
- `TrackChanged(Option<usize>)`, `PlaybackStarted/Paused/Stopped` — `play_track`, toggle (GUI/трей), stop, auto-advance (repeat-one-replay / исчерпание очереди).
- `QueueChanged` — load/scan/remove/clear/sort/drain-startup.
- `VolumeChanged(f32)` — слайдер громкости и wheel трея.
- `CoverChanged` — `drain_cover`.
- `DeviceChanged` — `set_output_device`.

**Потребители сейчас:** `drain_events` немедленно пушит свежее tray-состояние на завершение TrackChanged/Playback*/DeviceChanged (не дожидаясь троттлинга status-интервала); остальные события зарезервированы под будущие потребители (хоткеи, скробблы, визуализация).

**Почему не broadcast-шина:** single-owner архитектура (MusicApp единственный мутирующий владелец), детерминированный порядок обработки, нет потерь (feed дренится каждый тик), нет гонок подписки.

---

### 4.4. Семантика настроек: diff-apply и save-at-exit

**Статус:** ✅ сделано — eager-`settings.save()` убраны из volume (GUI, tray wheel), shuffle (заодно убран дубль `rebuild_shuffle`), repeat, sort prefs, save_column_widths. Сохранение `Settings` теперь происходит при выходе: оба выхода (tray Quit и close окна без minimize) вызывают `save_window_geometry()` → `settings.save()` (флашит всё). `on_settings_save` (Save в диалоге) — diff-apply: после `settings.settings = draft` переживает поля, не управляемые диалогом (volume/muted/last_dir/repeat/shuffle/sorted_col/sort_desc/win_*), чтобы не затереть live-изменения свежим значением из клона-драфта. Диалог настроек остаётся draft+cancel (закрытие без изменений).

**Проблема:** eager-save в каждом GUI/трей-хендлере: `settings.save()` в shuffle (mod.rs), volume (mod.rs GUI+wheel), column widths (ui_manager.rs), sort (playlist_manager.rs). Каждое движение слайдера/колеса = запись на диск.

**Задача:**
- `on_settings_save` применяет только **изменившиеся** поля (diff: theme / audio device / column widths / cover size) вместо тотальной перезаписи.
- Убрать eager-`settings.save()` из хендлеров (кроме случаев, где поле реально критично).
- Единый save-at-exit: закрытие окна + tray Quit → `settings.save()` (громкость, ширина колонок, сортировка доезжают при выходе).

---

### 4.5. Регулируемый сикбар: драг без дёрганья и seek при отпускании

**Статус:** ✅ сделано — сикбар генерирует seek только при отпускании. `ui/top_panel.slint`: прозрачный `TouchArea` (`seekgrab`) поверх `Slider` забирает клик/драг, на root свойства `seek-dragging`/`pending-seek`; слайдер показывает `pending-seek` во время драга, программные обновления тика (seek-fraction/pos/dur) при `seekbar-dragging` подавляются в `sync_playback_state_to_ui`. Новый callback `seek-commit(float)` (top_panel → app.slint) → `ui.on_seek_commit` (`src/app/mod.rs`): один seek в точку. `out property seekbar-dragging` проброшен в app.slint.

**Проблема:** Slint-слайдер `changed(v)` срабатывает и на программную установку `seek-fraction` тиком, и на перетаскивание — при воспроизведении ползунок «убегает» и seek выполняется десятки раз по ходу драга.

**Задача:** `TouchArea` поверх слайдера (`ui/top_panel.slint`), флаги `seekbar_dragging`/`pending_seek`, новый callback `seek-commit` (в `ui/app.slint`); тик не пишет `seek-fraction`/`pos` пока drag; seek — только при отпускании.

---

### 4.6. Громкость из трея в UI-ползунок без eager-save

**Статус:** ✅ сделано (пассивно): tray-wheel и GUI-слайдер меняют `player.set_volume()` и эмитят `AppEvent::VolumeChanged`; дельта-синк (`sync_playback_state_to_ui`) записывает `self.ui.set_volume(v)` на следующем тике если значение отличается от `last_ui.volume`. Tray push обновляется через `push_tray_status`. Быстрый пуш при `VolumeChanged` не нужен — троттлинг 100 мс достаточен.

**Проблема:** колесо трея меняет громкость, но слайдер громкости в окне не узнаёт об этом мгновенно (только через throttled tray/UI sync).

**Задача:** событие `AppEvent::VolumeChanged` → UI-синк обновляет ползунок; eager-`settings.save()` из wheel убрать (доедет на выходе, 4.4). Пункт 11 замечаний (wheel над слайдером громкости) — проверить `WheelEvent` в Slint `compat-1-2`; если нет — перенести в future-блок.

---

### 4.7. Порядок плейлиста на диске ≠ порядок просмотра

**Статус:** ✅ сделано — новое поле `disk_tracks: Vec<Track>` (порядок загрузки + скана) отделено от view-`tracks`. `save_playlist()` пишет `disk_tracks`; сортировка (`apply_sort`) переставляет только `tracks`. `disk_tracks` пополняется в `drain_startup_tracks`, `load_playlist`, `drain_scan` (оба конца), удаляется по пути в `remove_track`, очищается в `clear_playlist`. Eager-`save_playlist()` убраны из scan/remove/clear/sort. Сохранение на диск — только при выходе и если `queue_dirty`: tray Quit и закрытие окна (non-minimize). Явное «Save playlist» из GUI осталось (всегда сохраняет).

**Проблема:** `sort_tracks`/`apply_sort` переупорядочивают `self.tracks` и пишут плейлист на диск — сортировка разрушает исходный порядок файла.

**Задача:** параллельное поле `disk_tracks: Vec<Track>` (порядок загрузки + скана). `save_playlist()` пишет `disk_tracks` (не view-порядок). `save_playlist()` вызывается только при выходе (4.4) и только если `queue_dirty`. Ассеты: remove/all, clear, drain_scan, sort, exit-пути.

---

### 4.8. Баг-раунд: колесо трея/окна и диалог настроек

**Статус:** ✅ сделано — по замечаниям пользователя:
- **Трей-колесо** (`TrayCmd::Wheel`): квантование до одного «щелчка» — по знаку `delta` (`delta.signum()`), шаг 0.02; раньше сырая амплитуда KDE `±120` мгновенно ставила 0/max (Ubuntu слал `±1`).
- **Wheel над слайдером громкости в окне** (F3): `scroll-event` на обёртке-TouchArea вокруг вертикального Slider в `ui/top_panel.slint`; драг слайдера не трогается (свой TouchArea Slider ниже по дереву, wheel всплывает к обёртке). Знак: `delta-y > 0` = громче (winit: LineDelta вверх положителен), шаг 0.02.
- **Диалог настроек помнит неприменённые значения**: `on_open_settings` теперь вызывает `sync_settings_to_ui()` (все поля диалога всегда свежая реальность, а не стейль из прошлого draft/Save); Cancel-путь дополнительно сбрасывает вкладку Covers (`sync_cover_settings_to_ui`).
- **ComboBox устройств**: подсветка по фактическому `active_device` (устройство в использовании сейчас), затем сохранённая настройка, затем хостовый дефолт; placeholder `(loading…)`/idx=-1 — только при пустой модели (без «мигания первым элементом» при повторном открытии).

**Верификация:** `cargo build` ок; clippy baseline без изменений (16 lib + 10 bin); тесты 49 lib + 6 bin зелёные.

**Повторный раунд (замечания пользователя, повторное открытие):**
- **Диалог настроек помнил отменённые значения.** Причина: `Settings` в `app.slint` — один экземпляр, `open` лишь переключает `visible`; stateful-виджеты (ComboBox/Slider/CheckBox/LineEdit) держат внутреннее состояние и при взаимодействии перезаписывают one-way binding из корня (`changed model => reset-current()` в `combobox-base`, аналогично для value/checked), так что повторный Rust-resync их не сбрасывает. Фикс: содержимое диалога обёрнуто в `if root.open : VerticalLayout { ... }` (`ui/settings.slint`) — пересоздаётся на каждый open и читает свежие корневые пропы (Rust выставляет реальность до `set_settings_open(true)`). Cancel = `draft=None` + close.
- **ComboBox устройств показывал не активный девайс.** `changed model` переприсваивает `current-index` (рвёт binding), а `drain_audio_devices` ставил модель раньше индекса → `set_settings_device_idx(sel)` не доходил. Фикс: `ui_manager.rs` — индекс выставляется ДО модели (clamp сохранит корректное значение); энумерация pre-warmed в `MusicApp::init`, чтобы к первому открытию модель уже была.
- **Полярность/шаг** (`src/tray.rs:64` `Wheel(0 - delta)`, шаг окна 0.04): заданы пользователем (коммит `d570755`), менять нельзя.

**Round 3 (Edit-Commit hardening):** аудит нашёл реальные live-утечки draft → интерфейс:
- `on_settings_toggle_col` / `reset_cols` / `move_col_up` / `move_col_down` вызывали `sync_settings_to_ui()`, который писал **live**-пропы `cover-size`/`col-info-w`/`col-gap` (в `app.slint` они привязаны к TopPanel для верхней панели/альбома) из `settings_ref()` = **draft**. Сценарий: подвинуть ползунок обложки (draft), затем нажать кнопку колонок → плеер менялся вживую, Cancel всё не откатывал.
- `on_settings_theme_changed` вызывал ту же `sync_settings_to_ui()` (та же утечка).
- Фикс: выделен `sync_dialog_cols()` в `ui_manager.rs` (пересобирает только модель списка колонок диалога `settings-cols`); колончатые хендлеры переведены на него, theme-хендлер пишет только draft. `sync_settings_to_ui` теперь вызывается лишь в `init` (стартовые live-пропы из реальности) и `on_open_settings` (живые пропы = реальные значения, нетто-бездействие). live-пропы `cover-size`/`col-info-w`/`col-gap` меняются только в `on_settings_save`.
- Итог: при любых операциях в диалоге нет ни одного пути «draft → live»; Save применяет, Cancel теряет draft, пересоздание диалога (`if root.open`, round 2) показывает реальность.

---

### 4.9. Rework размеров колонок плейлиста (robust-распределитель с лимитами)

**Статус:** ✅ сделано — по спецe `_TODO_/COLUMN_WIDTH_IMPLEMENTATION.md` (переведена на реальную архитектуру; реальное ядро уже было: pct-модель, fit-to-window без h-scroll, драг в `StandardTableView` + debounce 2с, Reset, save-at-exit, валидация загрузки). Сделано заново то, чего не хватало:

- **Новый модуль `src/playlist_layout.rs`**: `ColumnLimit { min_px, max_px, max_pct }`, таблица `column_limit(id)` для всех 14 колонок (абсолютные `max_px` для коротких/числовых, относительные `max_pct` для текстовых; значения под ~900px view, «на глаз», тюнятся там же). Чистая функция `resolve_widths(container_w, ids, ratios)`:
  - идеал `ratio·W` (ratios нормируются, сумма не обязана быть 1) → клэмп `[min, min(max_px, W·max_pct)]`;
  - коррекция ≤7 итераций: `delta>0` — рост `∝(max−w)`, `delta<0` — сжатие `∝(w−min)`; всё зажато → равномерный спред (fallback по спеке);
  - дегенеративное окно (`W < Σmin`) — пропорциональное сжатие БЕЗ учёта min (интерфейс не ломается, warn в лог);
  - округление: вниз всё кроме последней, последняя поглощает остаток → сумма == W точно;
  - `round_fill` — отдельный helper; **+6 юнит-тестов** (узкое окно / caps / re-enable ребаланс / pinned-fallback / точность суммы / ratios≠1).
- **Интеграция** (`src/app/ui_manager.rs`): `build_table_columns` → `resolve_widths`; `save_column_widths_from_ui` перед px→pct клэмпит px-ширины драга границами `column_limit` → «резинка» при драге за грань, стабильный col-sig без пинг-понга debounce. Запись на диск — по-прежнему только при выходе (4.4, решение пользователя).
- **Заглушка** (`ui/playlist.slint`): при 0 видимых колонок — центрированный текст «Нет активных колонок». 
- **Reset** (`src/app/mod.rs` `on_settings_reset_cols`): сбрасывает **только ширины** к `default_column_width` (видимость/порядок сохраняются) — по решению пользователя (не «полный сброс», как было).

**Решения пользователя по дельтам спеки:** дефолты колонок — Rust-const (не в `config.toml`: нет UI для правки, при апдейте приложения значения в конфиге устареют); Reset widths-only; save-at-exit (не на драг-релиз); зажим после отпускания драга (встроенный виджет Slint не даёт жёстких границ во время драга; транзиентный h-scrollbar при овердраге возможен — возврат на debounce).

**Верификация:** build ок; clippy baseline 16 lib / 10 bin (не вырос); тесты 55 lib (49+6) + 6 bin зелёные. Осталась ручная проверка с дисплеем (драг, ресайз, скрыть все колонки) и тюнинг значений `column_limit()`.

---

### 4.10. Ресайз колонок и окна: пересчёт по релизу, а не по тикам

**Статус:** ✅ сделано — закоммичено (корневой фикс `max-width: 10000px` + пересчёт только после стабилизации ~64мс). Детали в коммите и `_STATE_.md`. Пользователь подтвердил («теперь работает!»); KDE 4K-проверка отложена.

**Осталось (живая проверка, нужен дисплей):**
- Ресайз окна мышью на Ubuntu GNOME FHD — проверить, что **рост быстрый и свободный**, колонки во время ресайза не двигают/не сжимают окно, а «дощёлкивают» после релиза (~64мс).
- **Сужение/расширение колонки драгом НЕ должно изменять размер окна** — ключевой признак фикса.
- Драг разделителя колонки — задержка после отпускания стала меньше.
- KDE 4K «упирается в границу» — подтвердить, что ушло (тот же max-лимит из корневого layout); если нет — проверить другие дочерние элементы с фиксированным max-width (TopPanel/StatusBar), geometry scale.

---

### 4.11. Маркер сортировки на колонке (▲/▼)

**Статус:** ✅ сделано — в этом коммите. Реализовано через встроенную механику material `StandardTableView`: `TableColumn.sort_order` (`slint::language::SortOrder`), виджет сам рисует стрелку, сам переключает её при клике (сбрасывает прежнюю колонку) и сам зовёт `sort-ascending`/`sort-descending`. Ручного добавления «▲» в title не нужно.

**Сделано:**
- `build_table_columns` (`src/app/ui_manager.rs`): `tc.sort_order` = Ascending/Descending/Unsorted из `settings.sorted_col`/`sort_desc`.
- Импорт `slint::language::SortOrder` в `src/app/mod.rs`.
- Цепочка обновления стрелок уже существовала: клик → `sort()` виджета → `on_sort_ascending/descending` → `sort_tracks` (обновляет настройки) → `sync_playlist_to_ui` пересобирает колонки → `set_vec` возвращает стрелку в согласованное состояние.

**Верификация:** build ок; тесты 55 lib + 6 bin зелёные; clippy без новых предупреждений от этого кода.

**Осталось (живая проверка):** стрелка показывает сохранённую сортировку при старте; клик по заголовку — стрелка ⟷ порядок треков; клик по другой колонке — стрелка переезжает; повторный клик — разворот.

**Отдельно (отложено пользователем):** уменьшить шрифт названий колонок — требует кастомизации material StandardTableView (копия виджета с меньшим `font-size` в заголовке) либо глобального `default-font-size`; решение за пользователем.

---

### 4.12. Конфиг-driven колонки плейлиста + now-playing + dbl-click + scroll-to-playing + кнопка repeat

**Статус:** ✅ сделано в коммите `CONFIG-driven-columns`.

**Сделано:**
- `ColumnCfg` (title/priority/min_width/max_width/max_width_percent/visible/column_type/width) + `default_columns()`; колонки живут в `settings.toml` → `[columns.*]`, миграция со старого TOML через `LegacySettings`/`migrate_legacy_columns`.
- NowPlaying-колонка (`column_type = "now-playing"`): маркер `▶` у текущего трека в `build_row`; `apply_sort`/`sort_rows_*` блокируют сортировку по ней.
- Bitrate отображается без «kbps».
- Play-кнопка при отсутствии декодера играет `current.unwrap_or(0)` (первый трек плейлиста) через новый `Player::has_decoder()`.
- Double-click = play, single-click = выделение (Rust-детект `last_click_row`+`last_click_time`, 400 мс).
- Scroll-to-playing: `Playlist.do-scroll-to-row` + `AppWindow.scroll-to-row` + вызов в `play_track` (флаг `scroll_to_playing`, default true).
- Одна кнопка repeat: цикл Off→All→One (иконки repeat.svg/repeat-one.svg), shuffle/repeat синхронизируются из конфига при старте.
- Reset колонок сбрасывает только ширины (к конфиг-приоритетам).

**Верификация:** всё проверено пользователем вживую: play → первый отображаемый трек играет, маркер ставится; режимы shuffle и repeat корректны.

---

### 4.13. Названия полей инфо-панели трека — в конфиг

**Статус:** ✅ сделано в коммите `info-panel-labels-in-config`.

**Сделано:**
- `Settings.info_labels: HashMap<String, String>` (стабильный ключ → строка) с дефолтной английской раскладкой
- `INFO_LABEL_KEYS` (13 ключей) + `default_info_labels()` + методы `info_label()` / `info_labels_ordered()`
- `TopPanel.info-labels: [string]` — одно свойство-модель вместо 13 захардкоженных строк; `InfoRow` индексируют её (`label: root.info-labels[0..12]`)
- `AppWindow.info-labels` проброс + `set_info_labels(...)` в `sync_settings_to_ui()` (init/open)
- 2 теста: дефолты в порядке отображения, override + fallback на дефолт/ключ
- Локализация/переименования — правкой `[info_labels]` в config.toml

---

### 4.2. Trait `AudioSource` с частичной реализацией (ISP)

**Статус:** ✅ сделано — `duration_secs` и `seek` стали default-методами (`src/audio/decoder.rs:25`): длительность выводится из `info().num_frames`, seek по умолчанию возвращает `Err("Seek is not supported…")`. Дублирующие impl убраны в Decoder/DsdDecoder/MockSource; +тест `optional_methods_have_safe_defaults`. Итого 49 lib + 6 bin.

**Файл:** `src/audio/decoder.rs`  
**Проблема (исходная):** все декодеры должны реализовывать все методы, даже если не поддерживаются.

**Сделано:**
- `seek() -> Result<(), String>` — default `Err("Seek is not supported by this audio source")`
- `duration_secs() -> Option<f64>` — default из `info().num_frames / info().sample_rate`
- Обязательные: `next_frames`, `info`, `eof` (+ `Send`)
- Реализации Decoder/DsdDecoder/MockSource сохранены (реальный seek), без дублей

---

### 4.3. Расширение тестового покрытия

**Статус:** ✅ сделано — тесты: `output.rs` (9, в 2.3: choose_output/MockHost), `player.rs` (13: переходы play/pause/stop/seek, volume/mute, EOF, lock-конфликт, snapshot). Итого 48 lib + 6 bin.
Остаётся (не приоритет): тесты менеджеров `app/` — можно добавить позже, отдельной мелкой задачей.

**Сделано:**
- `player.rs`: test-only конструктор `Player::test_new()` без cpal, `MockSource` по trait `AudioSource`, тесты переходов состояний.
- `output.rs`: `MockHost` + `choose_output` (закрыто в 2.3).

**Задача (оригинал):** Добавить тесты:
1. **Player:** тесты на переходы между состояниями (play/pause/stop/seek) — ✅
2. **Output:** тесты на выбор устройства (mock cpal) — ✅ (2.3)
3. **Managers:** тесты на взаимодействие компонентов — ⏳ отложено (низкий приоритет)

---

## Этап 5: Дополнительные улучшения (по желанию)

### 5.1. Логирование через `tracing` вместо `eprintln!`

**Статус:** ⬜ желательно — `eprintln!` на месте.

**Преимущества:**
- Настраиваемые уровни логирования
- Структурированные логи
- Возможность подключения к распределённой трассировке

---

### 5.2. Конфигурация через builder pattern

**Статус:** ⬜ желательно — `AppConfig` собирается напрямую из `Settings`.
```rust
let config = AppConfig::builder()
    .theme(Theme::Dark)
    .volume(0.8)
    .audio_device("default")
    .build();
```

---

### 5.3. Горячая перезагрузка конфигов — отклонено

**Причина:** конфиг (`SettingsStore`) меняется только из ГУИ; перечитывать файл с
диска на лету нечем — внешнего редактора/паттерна watch нет. При изменении настроек
из ГУИ изменения применяются сразу в `MusicApp`. `AppConfig::init()` (OnceLock-снапшот)
обновляется в `SettingsStore::save()` для читающих модулей (tray, cover, output).

---

## Future-блок (отложенные, без оценки по приоритету)

Ниже — задачи, которые сознательно отложены; НЕ удалять пункты 9/10 «визуализация».

### F1. Визуализация (анализатор/спектрограмма/осциллограмма)

**Статус:** ⬜ future — **устарел как черновик**: заменён детальным ТЗ 5.1 → **Этап 6**.

**Контекст:** в `ui/top_panel.slint:265-281` есть placeholder-блок (градиентный `Rectangle`) под будущий визуализатор. Развёрнутая спецификация — `_TODO_/done/ТЗ_5.1` (инбокс → **Этап 6**). Идеи ниже слиты в ТЗ 5.1 (tap в ring buffer, FFT в worker, `AppEvent`-feed, invoke_from_event_loop).

**Идеи для реализации (исторический контекст):**
- Сбор данных с аудио-выхода: семплы последнего фрейма/окна (частотный анализ через FFT в фоновом потоке), либо накопление в audio-колбэке.
- Пушить массив бар/уровней в Slint (property с массивом lengths: `in property <[length]> bars`, обновлять из тика с пониженной частотой ~30 fps).
- Событие-ветвление: форма с `AppEvent` уже есть (CoverChanged/PlaybackStarted и т.п.) — визуализатор может подписаться на feed.

### F2. Хоткеи (глобальные/локальные)

**Статус:** ⬜ future (заметка 12). Space/стрелки/цифры в окне + возможно глобальные. Потребитель события играет на `AppEvent`-feed (все транспорт-события уже эмитятся).

### F3. Wheel над слайдером громкости (заметка 11)

**Статус:** ✅ сделано (4.8) — Slint `compat-1-2` даёт `TouchArea.scroll-event(event) → EventResult`; обёртка вокруг Slider в `top_panel.slint` (+ шаг 0.02, знак delta-y>0 = громче). Драг слайдера не нарушен.

### F4. Тесты менеджеров `app/` (из 4.3, ⏳)

**Статус:** ⬜ future — модульные тесты `app/events.rs` (feed roundtrip, дельта-синк без AppWindow через заглушку UI-трейта), `playlist_manager` (disk/disk_tracks порядок).

### F0. Папка черновиков `_DRAFTS_/`

**Статус:** ✅ создана — папка для заданий «в процессе написания». Пользователь кладёт сюда файлы сам и переносит их в `_TODO_/` по мере готовности. Агент не превращает файлы из `_DRAFTS_` в задачи, пока они там. `визуализация.txt` (черновик задачи F1) перенесён сюда из `_TODO_/done/`.

## Этап 6: Визуализация аудио (ТЗ 5.1)

**Статус:** 🔶 в работе — **6.1 «Каркас» ✅**, **6.2 «Анализатор спектра» ✅**, **6.3 «DsdDecoder + ресемплеры» ✅**, **6.4 «Осциллограмма» ✅**, баг-фикс окно/layout визуализатора ✅ (см. ниже). **6.5 «Спектрограмма» — реализована, ждёт визуальной проверки пользователем (не закоммичено).**

### 6.5. Спектрограмма (полнотрековая) 🔶 реализовано, ждёт проверки

- **`src/audio/palettes.rs`** (новый): LUT magma/inferno/plasma/viridis (256, BIDS/colormap CC0) + `hex_for`/`parse_hex`; thermal (hot)/rainbow/gray — формулы в коде; solid = fg_color. +2 теста.
- **`src/audio/spectrogram.rs`** (новый): стриминговый FFT рендер частота×время, `columns×512` RGBA. `adaptive_plan(total, fft, max_frames)` → hop ≥ fft, columns ≤ max_frames; окна Hann/Hamming/Blackman; row→freq linear/log/mel (rows_per_ch, L верх/R низ при стерео); дБ уровень (mag/fft·sensitivity + gain_db + high_boost·log10(f/fmin)), нормировка range_db; `cic_compensation_db` — инверсия двух каскадных CIC ×8 (4-й порядок, норм. к DC, clamp 36 дБ, для DSD); палитра по уровню; feed/progress/finish (zero-pad). +7 тестов.
- **`src/audio/fulltrack.rs`**: `cache_key_spectrogram` (mode + все визуальные поля).
- **`src/app/fulltrack_manager.rs`**: диспетчер `run_build` → `run_osc`/`run_spec`; `run_spec` (DST/не-DST декодер, Spectrogram::new(…), feed, finish, PNG-кэш mode="spectrogram"); `drain_fulltrack`: want для mode 1|2, ключ по режиму, RAM-кэш `HashMap<key, Image>` (было по PathBuf), disabled-текст DSD оставлен только для осциллограммы; `FullEvt::Ready` без `path`.
- **`src/app/mod.rs`**: тип `fulltrack_cache` → `HashMap<String, Image>`.
- **`ui/visualizer.slint`**: слой изображения при `(mode==1||mode==2) && osc-ready`; градиент-заглушка спектрограммы убрана; плейсхолдер — пока `!osc-ready` для 1|2.
- **Верификация:** build + clippy (0 новых, baseline 22/9) + тесты 100 lib + 6 bin ✅; smoke на реальных файлах (примеры удалены): 10 c sweep 280 мс; «02. Rome.flac» 96 кГц/стерео/4:30 — 23.3 с декод symphonia (FFT ~40x realtime), 4000×512 = 8 МБ ≤ 128 МБ; пик лог-чирпа 200→8000 Гц отслежен по ярчайшей строке (бины сходятся).
- **Коммит:** после подтверждения пользователем (mode="spectrogram" в `[visualization]`).

### Баг-фикс: окно зажималось / layout визуализатора ✅ сделано

**Статус:** ✅ сделано — winit брал размер окна из WM-хинтов и резал до max ≈ 657 px (сумма max-лимитов фиксированных колонок top_panel); `horizontal-stretch: 1` у колонки визуализатора упирался в её собственный расчётный max (~119 px). Фикс обоих — приём «щедрый max-width»: `max-width: 100000px` на корне `AppWindow` (`ui/app.slint`) и на колонке визуализатора (`top_panel.slint`), col 3 обратно `width: 40px`. Осциллограмма: `image-fit: fill` вместо `contain` (картинка 16:1 в плейсхолдере ~6:1). Проверено xprop (user 1920×1008 / max 100000) и пользователем. Коммиты `ddeb176` + `6dc2dc5`.

### 6.4. Осциллограмма (полнотрековая) ✅ сделано

- **`src/audio/fulltrack.rs`** (lib): `Envelope` (min/max, stream), `render_rgba` (RGBA отрисовка, центр-линия, sensitivity, line_width, стерео/моно), `parse_color`, `cache_key` (path+mtime+size+mode+channels+параметры), disk cache (PNG + sidecar JSON) в `~/.cache/music_player/viz/`, `save_png`/`load_cached_png`/`cache_meta_valid`; 5 тестов.
- **`src/app/fulltrack_manager.rs`** (bin): `FullBuild`/`FullCmd`/`FullEvt`/`is_dsd`/`start_worker` (cancel через `AtomicBool`-флаг на sub-thread, disk-cache hit без декода, полный декод + прогресс каждые 64 пакета). `MusicApp::{setup_fulltrack, drain_fulltrack}` — авто-драйв, RAM-кэш, прогресс-бар, спец-плейсхолдер `skip_fulltrack_for_dsd` в DSD, фильтрация Ready по id/cache_key.
- **`src/app/mod.rs`**: модуль, поля, вызовы в new/tick.
- **Slint**: слой `Image` mode==1 (`osc-image`/`osc-ready`), плейсхолдер пока `!osc-ready`; проброс пропсов в top_panel/app.
- **Верификация:** 91 lib + 6 bin ✅, clippy 0 новых, build --release ок. Коммит `6dc2dc5`/`ddeb176` (совместно с баг-фиксом окна).

**Суть:** три режима визуализации (отключено/осциллограмма/спектрограмма/анализатор спектра), независимые настройки для каждого типа, стерео/моно, обработка DSD (PCM/native/DoP), bit-perfect, кэширование (RAM+диск), персистентность в config.toml.

**Архитектура:** `audio::visualizer` (обработка) + `ui::visualizer` (Slint-компонент); `FullTrackWorker` (полнотрековые, отдельный поток, та же конвертация, что у плеера — WYSIWYG) + `LiveWorker` (мгновенный, tap из cpal callback в ring buffer, FFT в отдельном потоке); настройки читаются атомарно на границе кадра (для LiveWorker — `RwLock<Arc<VisualizerConfig>>`, без arc_swap); UI-поток не считает FFT.

### 6.1. Каркас ✅ сделано

- **`src/audio/visualizer.rs`** (новый): `VisualizationMode`, `ChannelMode`, конфиги `OscilloscopeCfg`/`SpectrogramCfg`/`SpectrumCfg` с дефолтами по ТЗ §5, `VisualizerSettings` (`[visualization]` в config.toml), `VisualizerConfig::from_settings` (по образцу CoverConfig). +7 тестов (serde round-trip, дефолты, классификация режимов, partial-config fallback).
- **`src/settings.rs`**: поле `Settings.visualization` (`#[serde(default)]`) — вложенные `[visualization.<type>]` без flatten (TOML не поддерживает flatten).
- **Tap в ring buffer** (`rtrb 0.4.0`): `PlaybackCore.viz_tap: Option<Producer<f32>>` + `viz_tap_active`; после `fill()` во всех 3 колбэках (f32/i16/u8) копируется пост-ресемплерный PCM **до** громкости (`rtrb::push`, неблокирующий, drop на full). Методы `Player::set_viz_tap(Option<Producer>)` / `set_viz_tap_active(bool)`.
- **`ui/visualizer.slint`** (новый): компонент `Visualizer` — базовый плейсхолдер (артист/альбом/формат, §5.5), спец-плейсхолдер `disabled-text` (§6.3), градиент-заглушка активных режимов, полоса `build-progress`. Интегрирован в top_panel (замена градиентного блока), проброшены `viz-mode: int` через app.slint → TopPanel. `sync_settings_to_ui` пишет режим из конфига.
- **rtrb** из crates.io; `rustfft` 6.4.1 уже есть в lock (транзитивно от symphonia) — нужен будет на 6.2/6.4.
- **Верификация:** build ок, clippy — ноль новых warning, тесты 68 lib + 6 bin зелёные.

**Остальное (6.4+):** FullTrackWorker (осцилло/спектрограмма), DSD-режимы, bit-perfect, вкладка настроек «Визуализация/DSD/Звук», кэш RAM/диск.

### 6.3. Доработка DsdDecoder + Ресемплеры ✅ сделано

- **`src/settings.rs`**: новые секции `[dsd]` и `[audio]`/`[audio.resampler]` (ТЗ §8.2). `DsdMode (pcm/native/dop)`, `TargetBitDepth (16/24/32/32float)`, `TargetSampleRate (auto/44.1…192кГц)`, `ResamplerAlgorithm (linear/cubic/sinc_fast/…/sinc_slow, snake_case в TOML)`, `ResamplerDither (tpdf/triangular/off)`, `DsdCfg`, `AudioResamplerCfg`, `AudioCfg.bit_perfect`. Дефолты: sinc_medium, tpdf, pcm, 24 бит, auto. +4 теста (дефолты/round-trip/override/taps).
- **`src/audio/output.rs`** — `Resampler` переписан: `with_algo(src_rate, out_rate, src_ch, out_ch, algo)`; алгоритмы linear / cubic (Catmull-Rom, 4 точки) / sinc_fast/sinc_medium/sinc_slow (windowed-sinc, Hann, 32/64/128 тапов, DC-нормализация); буфер «base+pos» держит lookbehind для sinc (m-1) и cubic (1); EOF-хвост — клампинг окна к последнему кадру; **исправлен инвертированный коэффициент `ratio`** (было out/src вместо src/out — 44.1→48 кГц разгонял звук) и добавлен break по концу данных (`left >= base+frames`), чтобы pull не плодил мусор после исчерпания буфера. +5 тестов (identity, DC pass-through, ramp 2×, EOF-хвост, стерео→моно миксдаун).
- **`src/audio/player.rs`**: `Player.resampler_algo: ResamplerAlgorithm` (дефолт sinc_medium), `set_resampler_algorithm()`, `open()` создаёт `Resampler::with_algo`. `DsdDecoder` (CIC-децимация ×64, потоковая) не менялся — по §8.1 CIC фиксирован, не настраивается; `target_sample_rate`/`target_bit_depth` сохранены в конфиге, применяются на этапах 6.5/6.6 (device-caps/bit-perfect/dither).
- **`src/app/mod.rs`**: `player.set_resampler_algorithm(settings.audio.resampler.algorithm)` в `MusicApp::new`.
- **Верификация:** build + release ок, clippy — ноль новых warning (baseline 21 lib + 5 bin), тесты 86 lib + 6 bin зелёные.

### 6.2. Анализатор спектра (мгновенный) ✅ сделано

- **`src/audio/analyzer.rs`** (новый): `SpectrumEngine` (чистый DSP, без аудио-потока) — лестничный консьюмер rtrb, окно Ханна, FFT rustfft (2048, 50% overlap), полосы по шкалам linear/log/mel (20 Гц…min(nyquist,22 кГц)), сглаживание `cur=val·(1-sm)+prev·sm`, peak hold с `decay=exp(−1000/(decay_ms·fps))`, нормализация mag/`(fft_size·0.25)` и log-(dB scale, 0 dBFS…−80). + `LiveWorker` (поток-обёртка: config via `RwLock<Arc<VisualizerConfig>>`, атомики rate/in_ch, publication через `mem::swap` без аллокаций, пересборка движка по ключу (rate,in_ch,bands,channels), drain при mode≠spectrum). Константы `TAP_CAPACITY=262144`, `SPECTRUM_FFT_SIZE=2048`, `SPECTRUM_HOP=1024`. +9 тестов (тишина→0, тон в ожидаемой полосе, stereo L/R, mono-average, сглаживание, peak hold, покрытие бинов, worker drain+swap).
- **`rustfft 6.4.1`** — прямая зависимость (features avx/sse/neon).
- **`ui/visualizer.slint`**: компонент `SpectrumBars` (колонки через `horizontal-stretch`, bottom-anchor через `VerticalLayout alignment: end`, высота полосы = `clamp(value)·root.height` — без binding-цикла) и блок спектра (mode==3): стерео — L слева/R справа с зазором 4px (§4.2), моно — один ряд; `bar-gap`/`bar-radius`/`gradient` (§5.4); градиент `viz-3→viz-2→viz-1` по вертикали; старый градиент-плейсхолдер при mode==3 скрыт. Проброс пропсов `spectrum-l/-r`, `viz-channels`, `bar-gap/-radius`, `gradient` через `top_panel.slint` → `app.slint`.
- **`src/audio/player.rs`**: `Player::format() -> (u32, usize)` (out_rate/out_ch) для FFT.
- **`src/app/visualizer_manager.rs`** (новый, impl MusicApp): тап-кольцо + `LiveWorker` создаются в `new()` (`setup_visualizer`), `viz_push()` (~33 мс, `VIZ_PUSH_INTERVAL_MS=33` — отдельный `slint::Timer` в main.rs, не 100 ms tick): формат плеера → воркер, delta-применение конфига (sig: mode,bands,channels,gap,radius,gradient), тумблер tap (только при mode==spectrum), публикация полос L/R в `spectrum-l/-r` с delta, распад `×0.80` на паузе/стопе (обнуление <0.004).
- **Верификация:** build + release ок, clippy — ноль новых warning (baseline 21 lib + 5 bin), тесты 77 lib + 6 bin зелёные.

**Этапы разработки (из §16 ТЗ):**
1. ~~Каркас~~ ✅ 6.1 (готово)
2. ~~Анализатор спектра (мгновенный)~~ ✅ 6.2 (LiveWorker, FFT, полосы, сглаживание, peak hold + настройки + UI).
3. ~~Доработка DsdDecoder + ресемплеры~~ ✅ 6.3 (потоковая CIC-децимация фиксирована по §8.1, линейный/кубический/sinc_* ресемплеры, настройки `[dsd]`+`[audio]`, интеграция в PlaybackCore).
3. Доработка `DsdDecoder` + ресемплеры (linear/cubic/sinc_*/soxr) + интеграция в `PlaybackCore`.
4. Осциллограмма (полнотрековая): `FullTrackWorker`, min/max децимация, прогресс, отмена, кэш, `skip_fulltrack_for_dsd` — ✅ сделано в `594f56d`-следующий коммит 6.4.
5. Спектрограмма (полнотрековая): FFT по треку, палитры, адаптивный hop, CIC-компенсация — 🔶 реализовано, ждёт проверки.
6. Bit-perfect и DSD native/DoP: блокировка громкости, плейсхолдеры.
7. Полировка: seek по клику, персистентность и управление кэшем, метрики, тесты.
8. Документация.

**Ключевые ограничения производительности:** аудио-callback никогда не блокируется (try_push), 0 аллокаций в горячем цикле, FPS ≥ 30, live-worker ≤ 5% ядра, RAM полной спектрограммы ≤ 128 МБ.

**Полные критерии приёмки — в ТЗ §15** (зависимости: `rtrb`, `rustfft`; можно do без `arc_swap` через `Arc<Mutex<>>`+clone).

**Взаимодействие с плеером:** сигналы TrackChanged/StateChanged/PositionChanged/Seek/BitPerfectChanged/DsdModeChanged/DeviceChanged/SettingsChanged — слой `AppEvent` уже готов (п. 4.1), потребитель подключается на feed.

---

## Сводная таблица приоритетов

| # | Задача | Приоритет | Статус | Оценка времени | Риск регрессии |
|---|--------|-----------|---------|----------------|----------------|
| 1.1 | Безопасная обработка lock() в player.rs | 🔴 Критический | ✅ сделано | 2ч | Низкий |
| 1.2 | Исправление гонок в cover.rs/playlist.rs | 🔴 Критический | 🔶 частично | 1ч | Низкий |
| 1.3 | Неблокирующий probe устройства | 🟡 Высокий | ✅ сделано | 2ч | Средний |
| 2.1 | Разделение MusicApp на сервисы | 🟡 Высокий | ✅ сделано | 8ч | Высокий |
| 2.2 | Dependency Injection | 🟡 Высокий | ✅ сделано | 4ч | Средний |
| 2.3 | Trait AudioHost | 🟢 Средний | ✅ сделано | 3ч | Средний |
| 3.1 | Оптимизация сортировки | 🟢 Средний | ✅ сделано | 2ч | Низкий |
| 3.2 | Хэш-субдиректории кэша | 🟢 Средний | ✅ сделано | 1ч | Низкий |
| 3.3 | Асинхронная загрузка плейлиста | 🟢 Средний | ✅ сделано | 2ч | Средний |
| 4.1 | Event-driven архитектура | 🔵 Низкий | ✅ 4.1 | 6ч | Высокий |
| 4.2 | ISP для AudioSource | 🔵 Низкий | ✅ | 3ч | Средний |
| 4.3 | Расширение тестов | 🔵 Низкий | ✅ | 8ч | Низкий |
| 4.4 | Семантика настроек: diff-apply + save-at-exit | 🔵 Низкий | ✅ | 4ч | Средний |
| 4.5 | Сикбар: драг + seek при отпускании | 🔵 Низкий | ✅ | 3ч | Средний |
| 4.6 | Трей-громкость → UI-ползунок | 🔵 Низкий | ✅ | 1ч | Низкий |
| 4.7 | Порядок на диске ≠ порядок просмотра | 🔵 Низкий | ✅ | 2ч | Средний |
| 4.8 | Баг-раунд: tray/окно wheel + настройки + ComboBox | 🔵 Низкий | ✅ | 3ч | Низкий |
| 4.10 | Ресайз колонок/окна: пересчёт по релизу | 🔵 Низкий | ✅ сделано | 3ч | Средний |
| 4.11 | Маркер сортировки на колонке (▲/▼) | 🔵 Низкий | ✅ сделано | 1ч | Низкий |
| 4.12 | Конфиг-driven колонки + now-playing + dbl-click + scroll + repeat | 🔵 Низкий | ✅ сделано | 8ч | Средний |
| 4.13 | Названия полей инфо-панели в конфиг | 🔵 Низкий | ✅ сделано | 1ч | Низкий |
| 5.1 | tracing вместо eprintln! | ⬜ желательно | ⬜ | — | Низкий |
| 5.2 | builder для AppConfig | ⬜ желательно | ⬜ | — | Низкий |
| 5.3 | Горячая перезагрузка конфигов | — отклонено | 🚫 | — | — |
| 6.1 | Визуализация аудио (ТЗ 5.1): каркас + спектр + DSD/resamplers + oscillo/spectrogram + bit-perfect | 🔴 Высокий (ТЗ от пользователя) | 🔶 6.1 ✅; 6.2 ✅; 6.3 ✅; 6.4 ✅; 6.5 🔶 (ждёт проверки); 6.6+ ⬜ | 8 этапов (§16) | Высокий |

---

## Рекомендуемый порядок выполнения

```
Этап 1 (стабильность) → Этап 2 (архитектура) → Этап 3 (производительность) → Этап 4 (тестируемость)
```

**Важно:** Не начинать Этап 2, пока не завершён Этап 1. Рефакторинг нестабильного кода приведёт к переносу багов в новую архитектуру.

---

## Чеклист для каждого шага

Для каждой задачи:
- [ ] Написать тесты на текущее поведение (если возможно)
- [ ] Сделать изменения минимальными
- [ ] Проверить компиляцию без предупреждений
- [ ] Запустить существующие тесты
- [ ] Обновить документацию (если изменился API)

---

## Примечания для OpenCode

При выполнении задач:
1. Следовать принципам **Rust idioms** (Result, Option, unwrap_or_else)
2. Избегать `unwrap()` в production коде
3. Сохранять обратную совместимость там, где это возможно
4. Добавлять комментарии для сложных участков кода
5. Обновлять `ROADMAP.md` после завершения каждого этапа
