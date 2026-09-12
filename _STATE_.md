# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммится вместе с изменениями кода.

## Статус

- **Состояние:** `done` — Этап 6, подзадача **6.2 «Анализатор спектра (мгновенный)»** завершена (ТЗ §16.2).

## Активная задача

**6.2 — LiveWorker: FFT + полосы + сглаживание + peak hold.**
- `rustfft` как прямая зависимость;
- `src/audio/analyzer.rs`: `SpectrumEngine` (чистый DSP, тестируемый) + `LiveWorker` (поток-консьюмер rtrb);
- живой снапшот настроек `RwLock<Arc<SpectrumCfg>>` (решение по RwLock из 6.1);
- отрисовка полос в `ui/visualizer.slint` (mode==3, mono/stereo по ТЗ §4.2: L слева / R справа);
- интеграция в `MusicApp`: `viz_manager.rs`, создание tap в `new()`, `sync_viz()` в `tick()`, распад полос на паузе/стопе;
- старый градиент-плейсхолдер скрывается при mode==3.

## Шаги

- [x] 1. Cargo.toml: `rustfft`
- [x] 2. `src/audio/analyzer.rs`: `SpectrumEngine` + `LiveWorker` + тесты (синтез: тон/шум/стерео)
- [x] 3. `src/audio/mod.rs`: подключить `analyzer`
- [x] 4. `ui/visualizer.slint`: `spectrum-l/spectrum-r/viz-channels` + `BarRow` (полосы L/R)
- [x] 5. `ui/top_panel.slint` + `ui/app.slint`: проброс spectrum-пропсов
- [x] 6. `src/audio/player.rs`: метод `Player::format()` (out_rate/out_ch)
- [x] 7. `src/app/visualizer_manager.rs` (новый, impl MusicApp): создание LiveWorker+tap в `new()`, `sync_viz()`/`push_viz_to_ui()` в `tick()`
- [x] 8. build + clippy + test → коммит

## Изменяемые файлы

- `Cargo.toml`/`Cargo.lock` — rustfft
- `src/audio/analyzer.rs` (новый), `src/audio/mod.rs`
- `ui/visualizer.slint`, `ui/top_panel.slint`, `ui/app.slint`
- `src/audio/player.rs`
- `src/app/visualizer_manager.rs` (новый), `src/app/mod.rs`
- `ROADMAP.md`, `_STATE_.md`

## Риск / стоп-условие

- Аудио-callback не трогаем (tap уже есть, только `Player::format()` — блокирующий lock, но только в UI-потоке).
- Воркер не выделяет память в горячем цикле (переиспользуемые буферы `window`/`work`).
- Slint: высоты полос через `% * f32`; проверить компиляцию.

## Следующий ход

Задача закрыта. Следующая: **6.3** — доработка `DsdDecoder` + ресемплеры (linear/cubic/sinc_*/soxr) + интеграция в `PlaybackCore` (ТЗ §16.3). Начать с `_TODO_`/ROADMAP.