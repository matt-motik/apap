# Текущая микро-сессия

- **Задача из ROADMAP:** A2.0-5.1+5.3 — `PlaybackWorker` (Producer) + колбэки-Consumer через lock-free ring (спека `docs/spec_audio_core_v2.0.md` §5.1/§5.3)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/worker.rs` (новый)
  - `src/audio/mod.rs`
  - `src/audio/player.rs`
  - `src/audio/output.rs`
  - `src/app/visualizer_manager.rs`
- **Критерий успеха (Definition of Done):** декодирование и ресемплинг вынесены в поток-воркер; cpal-колбэки читают только `RtConsumer` (ring + атомики), без `Mutex`; `Player` API без регрессий; полный `cargo test` + `clippy` зелёные, 0 новых предупреждений.

## Итерационный трекер
[x] Шаг 1: `worker.rs` — `RtShared` (атомики + геометрия) и `RtConsumer` (ring + scratch), `mod worker`. Проверка: `cargo test audio::worker` — 6 тестов зелёные.
[x] Шаг 2: `PlaybackWorker` + `WorkerCmd` (Seek/SetVizTap/Stop) + `worker_loop` + seek-хендшейк/eof/viz-tap, `stop_requested` против дедлока join. Проверка: 7 тестов worker (E2E `worker_fills_ring_with_exact_frames`, seek) зелёные.
[ ] Шаг 3: RT-колбэки `audio_callback_*_rt(&mut RtConsumer, data)` в `player.rs`: pull из ring, volume/mute/bit-perfect/dither/формат из `RtShared`; тесты форматирования из заранее наполненного ring. Проверка: `cargo test audio::player` зелёный.
[ ] Шаг 4: `output.rs` — `build_stream_rt(spec, RtConsumer, error_flag)` и адаптация `probe_output`. Проверка: `cargo check` + тесты `audio::output`.
[ ] Шаг 5: `Player` на worker+RtShared+RtConsumer; удалить `PlaybackCore` и старые колбэки; переписать player-тесты (включая zero-alloc детектор). Проверка: полный `cargo test` + clippy без новых предупреждений.

- **Текущий шаг (current_step):** Шаг 3
- **Следующий ход:** Добавить `audio_callback_*_rt` в `src/audio/player.rs`.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress