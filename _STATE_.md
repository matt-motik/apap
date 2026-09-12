# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммится вместе с изменениями кода.

## Статус

- **Состояние:** `done` — Этап 6 подзадача **6.1 «Каркас»** доведена до рабочего состояния и закоммичена.

## Активная задача

Следующая — **6.2 «Анализатор спектра (мгновенный)»** (LiveWorker + FFT + полосы, ТЗ §16.2). Перед стартом завести в этом файле.

## Выполнено

### 6.1 «Каркас визуализации»
- `src/audio/visualizer.rs` (новый): режимы + конфиги трёх типов + дефолты по ТЗ §5, `VisualizerSettings`, `VisualizerConfig::from_settings`. +7 тестов.
- `src/settings.rs`: `Settings.visualization` (`#[serde(default)]`, вложенные `[visualization.<type>]`).
- Tap: `PlaybackCore.viz_tap: Option<rtrb::Producer<f32>>` + `viz_tap_active`; пост-ресемплерный PCM до громкости во всех 3 колбэках (неблокирующий `push`). Методы `Player::set_viz_tap/set_viz_tap_active`.
- `ui/visualizer.slint` (новый) + интеграция в top_panel/app.slint (prop `viz-mode`), `sync_settings_to_ui`.
- Зависимость `rtrb` 0.4.0.
- Решение по вопросу пользователя про `Arc<RwLock<>>`: `PlaybackCore` остаётся `Mutex` (callback мутирует — эксклюзив); снапшот настроек для LiveWorker — `RwLock<Arc<VisualizerConfig>>` на 6.2.

## Изменяемые файлы

- `Cargo.toml`/`Cargo.lock` — rtrb
- `src/audio/visualizer.rs` (новый), `src/audio/mod.rs`
- `src/settings.rs`
- `src/audio/player.rs`
- `ui/visualizer.slint` (новый), `ui/top_panel.slint`, `ui/app.slint`
- `src/app/ui_manager.rs`
- `ROADMAP.md` (6.1 ✅), `_STATE_.md`

## Верификация

- `cargo build` — ок; `cargo test` — 68 lib + 6 bin зелёные; clippy — ноль новых warning (baseline 21 старых).
- Живая проверка с дисплеем (режим отображения, 4К-проверка) — после 6.2, когда появится реальная отрисовка.

## Риск / стоп-условие

- Аудио-callback не блокируется (только `push` без блокировок/аллокаций) — соблюдено.
- TOML не поддерживает `serde(flatten)` — общие поля дублируются в каждом конфиге.