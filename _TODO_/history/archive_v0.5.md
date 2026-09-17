# Архив реализации: Спека A2.0 — Real-Time аудио-ядро (харденинг)

> Файл создан при чистке ROADMAP.md 2026-09-17.
> Все коммиты влиты в `main`. Спека A2.0 закрыта полностью, открытых подзадач нет.
> Спека: `docs/spec_audio_core_v2.0.md` (префикс `A2.0`, согласована в чате).
> Фазирование: A2.0-3 → A2.0-4 → A2.0-5 (по §7 спеки).

---

## 1. Итог спеки A2.0

- **A2.0-3 «0 аллокаций в RT-колбэках»**: фиксированный преаллоцированный
  scratch-потолок, guard «не растить» (тишина вместо resize), запрет `shrink_to`,
  тест-детектор аллокаций через `#[global_allocator]`.
- **A2.0-4 «Честная индикация bit-perfect»**: состояние
  `bit_perfect_resampled` (bit-perfect активен, но устройство ресемплит) +
  UI-бейдж «Resample (device limit)» в статус-баре.
- **A2.0-5 «Producer/Consumer»**: декодирование вынесено из RT-потока
  (`PlaybackWorker` → `rtrb`-ring → `RtConsumer`); управляющее состояние — только
  атомики; seek-хендшейк по поколениям с дренажем ring; пауза/стоп/EOF на
  воркере; DoP passthrough через ring; пересоздание worker+ring при смене
  устройства/трека; настраиваемая глубина ring (`ring_buffer_ms`).

## 2. Технический паспорт (шаг → коммит → что сделано)

| Этап | Коммит | Дата | Технические детали |
| --- | --- | --- | --- |
| 3.1 | `12005b0` | 2026-09 | Преаллокация `scratch` в `open_pcm`/`open_dop` до `MAX_OUT_SAMPLES`; `RtConsumer` держит `Vec<f32>` фиксированной ёмкости. |
| 3.2 | `45545e5` | 2026-09 | Guard «не растить» во всех `audio_callback_*`: запрос > потолка → тишина вместо `resize`. |
| 3.3 | `450d488` | 2026-09 | Запрет `shrink_to` в `scratch_release` (только `clear()`). |
| 3.4 | `c3334fd` | 2026-09 | `alloc_tracking.rs` + `#[global_allocator]`: счётчик аллокаций в куче; тест zero-alloc в колбэках. |
| 4.1 | `954bafb` | 2026-09 | Флаг `bit_perfect_resampled` в `Player` (bit-perfect + активный ресемплер) → WARN в лог. |
| 4.2 | `435575b` | 2026-09 | UI-бейдж «Resample (device limit)» в статус-баре (`playback_manager.rs`), проверка `bit_perfect_resampled()`. |
| 5.1 | `894c497` | 2026-09-16 | `worker.rs`: `RtShared`/`RtConsumer`/`PlaybackWorker`/`WorkerCmd`/`worker_loop`/`produce_chunk`/`push_all`; `mod.rs += pub mod worker;`. |
| 5.3 | `7b78b91` | 2026-09-16 | RT-колбэки `audio_callback_*_rt(&mut RtConsumer, …)` + `build_stream_rt` в `output.rs`; `probe_output` на новом пути. |
| 5.1+5.3..5.8 | `2c32b51` | 2026-09-16 | `Player` полностью переведён на worker+RtShared+RtConsumer; `PlaybackCore` и старые Mutex-колбэки удалены; `playback_manager.rs` — без `player.core.lock()`. |
| 5.2 (settings) | `9714376` | 2026-09-17 | `AudioCfg.ring_buffer_ms: u32` (serde-default), `RING_BUFFER_MS_DEFAULT/MIN/MAX`, `clamp_ring_buffer_ms`, TOML-roundtrip-тесты. |
| 5.2 (player) | `263bd9c` | 2026-09-17 | `Player.ring_buffer_ms` + `set_ring_buffer_ms`; `ring_capacity` (ms→samples + floor 2× cpal-буфера); проброс из `settings.audio` в `app/mod.rs`. |

## 3. Ключевые структуры данных и константы

- **`#[global_allocator]` (alloc_tracking)** — счётчик аллокаций; в RT-колбэках 0.
- **`MAX_OUT_SAMPLES = 1 << 16`** — фиксированный потолок scratch (одна структура на всё, `pub const` в `worker.rs`).
- **`WORKER_CHUNK_FRAMES = 1024`** — порция декодирования воркером (стаггинг-буфер `vec![0.0; chunk × out_ch]`).
- **Ring-буфер**: ёмкость `max(out_rate × out_ch × ms / 1000, 2 × cpal_buffer_frames × out_ch, 4096)`;
  `ring_buffer_ms` default 1500, диапазон `[100..10000]`; `BufferSize::Fixed` → floor 2 периода, `Default` → floor 0 (4096).
- **`RtShared` (без `Mutex`)**: атомики `volume_bits: AtomicU32`, `muted`, `playing`, `bit_perfect`, `dither: AtomicU8` (индекс), `viz_tap_active`, `pos_frames: AtomicU64`, `eof`, `natural_end`, `seek_generation: AtomicU64`, `seek_done_generation`, `stop_requested`, `rt_err: AtomicU8`; immutable-геометрия `out_rate`, `out_ch`, `resampler_enabled`. Все обращения — `Ordering::Relaxed` (колбэк read-only).
- **`TpdfRng`** — zero-alloc xorshift32 LCG (запрет `rand::thread_rng()`); seed на трек из хэша пути (`track_seed`); TPDF = `u1 + u2 − 1`.
- **Dither-индексы**: `DITHER_INDEX_TPDF = 0`, `DITHER_INDEX_TRIANGULAR = 1`, `DITHER_INDEX_OFF = 2`; `dither_amplitude`: TPDF = 1 LSB, triangular = 0.5 LSB.
- **Seek-хендшейк**: UI `begin_seek` → `WorkerCmd::Seek(target_frames)`; воркер `source.seek()` + `resampler.reset()` + `set_eof(false)` + `publish_seek_done`; consumer `reconcile_seek()` дренит ring и ре-бейзит `pos_frames`.
- **Viz-tap**: `VizTap = Arc<Mutex<Option<rtrb::Producer<f32>>>>` — переживает пересоздание воркера (lock короткий, вне RT).

## 4. Верификация

- Тесты: `cargo test` — 172 lib + 13 bin зелёные (включая zero-alloc детектор
  `all_callbacks_perform_zero_allocations`, seek/reconcile, RT-форматирование
  f32/i16/u8/i32-PCM/i32-DoP, ms→samples ring-капюлатора).
- `cargo test audio::player` — 27 тестов (в т.ч. `ring_capacity_*`,
  `set_ring_buffer_ms_clamps`).
- `cargo clippy` — 30 warning (базовая линия 33: старый Mutex-путь и
  clone-on-Copy убрали 3; новых в изменённых файлах нет).
- Ручная приёмка (2026-09-17): воспроизведение, seek, play/pause — ок.