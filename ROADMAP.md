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

**Статус:** ⬜ не сделано — шины событий нет, компоненты связаны напрямую.

**Проблема:** Прямые вызовы между компонентами создают сильную связанность.

**Задача:** Внедрить шину событий:
```rust
pub enum AppEvent {
    TrackChanged(usize),
    PlaybackStateChanged(bool),
    CoverLoaded(u64, Option<PathBuf>),
    // ...
}

pub struct EventBus {
    tx: broadcast::Sender<AppEvent>,
}
```

**Преимущества:**
- Компоненты не знают друг о друге
- Легче добавлять новые фичи
- Упрощается тестирование (можно подписаться на события)

---

### 4.2. Trait `AudioSource` с частичной реализацией (ISP)

**Статус:** 🔶 частично — `AudioSource` есть (`src/audio/decoder.rs:25`), но все методы обязательные (`next_frames/seek/duration_secs/info/eof`), default-методов для необязательных операций нет.

**Файл:** `src/audio/decoder.rs`  
**Проблема:** Нарушение **Interface Segregation Principle** — все декодеры должны реализовывать все методы, даже если не поддерживаются.

**Задача:**
```rust
pub trait AudioSource {
    fn info(&self) -> &AudioInfo;
    fn read_samples(&mut self, buffer: &mut [f32]) -> Result<usize>;
    
    // Опциональные методы через trait extension
    fn seek(&mut self, pos: f64) -> Result<()> {
        Err(Error::NotSupported)
    }
}
```

---

### 4.3. Расширение тестового покрытия

**Статус:** 🔶 частично — тесты добавлены (cover, settings, playlist, audio/dsd, audio/decoder, app/mod — 23 lib + 6 bin). Нет тестов: `player.rs`, `output.rs`, менеджеров `app/`.

**Текущее состояние:** Тесты есть для `playlist.rs`, `cover.rs`, но отсутствуют для:
- `player.rs` (критично!)
- `output.rs`
- `app/` менеджеров

**Задача:** Добавить тесты:
1. **Player:** тесты на переходы между состояниями (play/pause/stop/seek)
2. **Output:** тесты на выбор устройства (mock cpal)
3. **Managers:** тесты на взаимодействие компонентов

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
| 4.1 | Event-driven архитектура | 🔵 Низкий | ⬜ | 6ч | Высокий |
| 4.2 | ISP для AudioSource | 🔵 Низкий | 🔶 частично | 3ч | Средний |
| 4.3 | Расширение тестов | 🔵 Низкий | 🔶 частично | 8ч | Низкий |
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
