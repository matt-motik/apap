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

**Статус:** ⬜ — задача из инбокса `_TODO_/playlist.txt` (внешний ИИ-анализ).

**Желаемое поведение (от пользователя):**
1. Изменение столбца: тянешь мышкой, отпустил — произошёл пересчёт колонок, без перерисовки каждый тик.
2. Любое изменение геометрии окна (кнопкой, мышью) — тоже пересчёт в КОНЦЕ (на отпускание кнопки мыши), а не на каждом тике движения мыши.
   - Ок: `save_window_geometry()` вызывается только при закрытии (это задумано) — не трогать.

**Вопрос пользователя, требующий проверки:** «Почему не получается ухватить мышкой за правый край и изменить размер окна?» — нужно проверить, почему resize окна мышью не работает (возможно, `preferred-width/height` или конфликт layout в `ui/app.slint`, либо обработка платформой).

**Задачи:**
- Добавить обработку `columns-changed` от `StandardTableView` (или аналог) для немедленного пересчёта ширин на отпускание.
- Обработчик изменения размера окна (resize)→ пересчёт колонок на релиз мыши, не на тик.
- Разобраться с проблемой ухватывания правого края окна мышью.

**Прил. (диффы от внешнего ИИ, `_TODO_/ui_playlist_diff.slint` и `_TODO_/ui_app_diff.slint`):** предлагается callback `columns-changed([TableColumn])` в `ui/playlist.slint`, проброшенный в `ui/app.slint` как `playlist-columns-changed` (подписаться в Rust для пересчёта ширин). **Важно:** такой callback **не существует** в Slint 1.17.1 (проверено в `i-slint-compiler/widgets/*/tableview.slint` — нет `columns-changed`; ширина меняется во внутреннем `adjust_size`, сигнала на отпускание нет). Диффы неприменимы напрямую — используются как референс желаемого поведения.

**Сделано (решение пользователя — «Плавный пересчёт только ширин»):**
- `MusicApp.playlist_cols: Rc<VecModel<TableColumn>>` — постоянная модель колонок, подключается к UI один раз (`src/app/mod.rs`).
- `update_column_widths` (`src/app/ui_manager.rs`) → точечные `set_row_data(i, col)` только при изменении `.width`; `set_vec` — только при смене состава/порядка/видимости. Ресайз окна больше не пере-создаёт `ModelRc` (плавно, без рывков; ширины колонок не зажимают окно).
- `sync_playlist_to_ui` → `playlist_cols.set_vec(cols)`.
- **Стабилизационный рефлоу по ширине окна** (`VIEW_W_SETTLE_TICKS=3`, ~300 мс): колонки пересчитываются не каждый тик при ресайзе окна, а после стабилизации `visible-width`. Фикс «медленного роста» окна (перезапись ширин вживую боролась с растягиванием Window и оно ползло ~раз в полсекунды).
- **Сокращён debounce драга колонки** (20→6 тиков, ~600 мс): корректировка мин/макс после отпускания заметно быстрее (было ~2 с).

**Осталось (живая проверка, нужен дисплей):**
- Ресайз окна мышью на Ubuntu GNOME FHD — проверить, что рост стал быстрым, а сжатие осталось плавным (колонки «дощёлкивают» через ~300 мс после остановки).
- Драг разделителя колонки — задержка после отпускания стала меньше.
- KDE 4K «упирается в границу» — подтвердить, что ушло; если нет — диагностика slint-window resize limits / scale geometry.

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

**Статус:** ⬜ future.

**Контекст:** в `ui/top_panel.slint:265-281` есть placeholder-блок (градиентный `Rectangle`) под будущий визуализатор. Пользователь добавит в репозиторий папку с Python-примером (как выглядит) и файл описания желаемого поведения.

**Идеи для реализации:**
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
| 4.10 | Ресайз колонок/окна: пересчёт по релизу | 🔵 Низкий | ⬜ план | 3ч | Средний |
| 5.1 | tracing вместо eprintln! | ⬜ желательно | ⬜ | — | Низкий |
| 5.2 | builder для AppConfig | ⬜ желательно | ⬜ | — | Низкий |
| 5.3 | Горячая перезагрузка конфигов | — отклонено | 🚫 | — | — |

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
