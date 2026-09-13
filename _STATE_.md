# Текущая микро-сессия

- **Задача из ROADMAP:** 6.8 — Bit-perfect и DSD native/DoP (Блок A + B-заглушка; DoP-MVP во вторую очередь)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/player.rs`
  - `src/audio/output.rs`
  - `src/audio/dsd.rs`
  - `src/audio/dop.rs` (новый — Блок C)
  - `src/audio/mod.rs` (регистрация модуля dop)
  - `src/settings.rs`
  - `src/app/mod.rs`
  - `src/app/ui_manager.rs`
  - `src/app/playback_manager.rs`
  - `src/app/events.rs`
  - `src/app/fulltrack_manager.rs`
  - `src/tray.rs`
  - `ui/top_panel.slint`
  - `ui/app.slint`
  - `ROADMAP.md`
  - `_STATE_.md`
- **Критерий успеха (Definition of Done):** Блок A (A1–A10), фикс F1, заглушка B1 и DoP MVP (C1–C5) реализованы; `cargo check`/`cargo clippy`/`cargo test` зелёные.

## Блок A — Bit-perfect PCM (текущая фаза)

- [x] A1: TpdfRng (LCG, zero-alloc, next_f32 в [-1,1], seed на open). Тест: tpdf_samples_stay_in_unit_range + tpdf_mean_is_zero. Файлы: player.rs
- [x] A2: PlaybackCore: bit_perfect: Arc<AtomicBool>, dither: Arc<AtomicU8>, tpdf: TpdfRng. Файлы: player.rs
- [x] A3: Байпас громкости в f32-колбэке (bit_perfect || (vol>=1.0 && !muted)). Тест: bit_perfect_bypasses_software_volume + passthrough. Файлы: player.rs
- [x] A4: TPDF-дизеринг в i16/u8 конверсии (skip только при bit-perfect native-pute), round вместо truncate. Тест: tpdf_dither_adds_noise_to_quantization. Файлы: player.rs
- [x] A5: Resampler pre-allocation (MAX_BUFFERED_FRAMES=8192, capacity в with_algo, guard в push). Тесты: 14 output-теста зелёные. Файлы: output.rs
- [x] A6: Player::set_bit_perfect(bool) + set_dither(ResamplerDither); MusicApp::new прокидывает из settings. WARN-лог при rate-mismatch в bit-perfect. Файлы: player.rs, app/mod.rs
- [x] A7: AppEvent::BitPerfectChanged → drain_events пушит tray. Файлы: events.rs, app/mod.rs
- [x] A8: UI: top_panel.slint CheckBox "BP" + enabled на Slider/Mute; app.slint prop/callback; sync через sync_playback_state_to_ui. Файлы: top_panel.slint, app.slint, playback_manager.rs
- [x] A9: Tray: TrayState.bit_perfect, колесо громкости игнор при bit_perfect. Файлы: tray.rs, app/mod.rs
- [x] A10: Persist: eager-save settings.audio.bit_perfect при toggle (v=1.0 + unmute при включении). Файлы: app/mod.rs

## Блок B — DSD Native (заглушка)

- [x] B1: Player::open() при dsd.mode==Native → Err("DSD native output not supported by cpal") + WARN в лог; set_dsd_mode + проброс из MusicApp::new. Файлы: player.rs, app/mod.rs

## Блок C — DoP MVP (вторая очередь, после подтверждения Блока A)

- [x] C1: Raw-DSD passthrough в DsdDecoder (DecodeMode::Dop, open_with_mode; de-interleave DSF planar / passthrough DFF; буферы предвыделены — 0 аллокаций в hot loop)
- [x] C2: DoP-фреймер (dop.rs): 16 бит + маркеры 0x05/0xFA, DoPFramer.marker_phase. Тесты: dop_marker_pattern + dop_phase_continues_across_calls
- [x] C3: audio_callback_i32 (DoP) — Direct Output без громкости/дизеринга; build_stream SampleFormat::I32 (был unreachable)
- [x] C4: Player::open_dop(): dop_rate = DSD/8 (для DSD64 352.8 кГц, бит-точный; НЕ src_rate*2 — отступление задокументировано); проверка rate×ch устройства; fallback PCM (CIC) с WARN при несовпадении или сбое билда/старта. Файлы: player.rs, output.rs, dsd.rs
- [x] C5: Интеграция через settings.dsd.mode → Player::open(); переход в 6.8b — UI-переключатель и отладка бит-потока на реальном устройстве

## Фикс из ревью 6.7

- [x] F1: skip_fulltrack_for_dsd блокирует и спектрограмму (текст заглушки). Файлы: fulltrack_manager.rs

## ROADMAP-update (в конце фазы Блока A)

- [ ] R1: 6.8 ← добавить TPDF/dither + валидация DSD target; 6.9 ← курсор, CIC-компенсация, seek по клику, тесты §15; Future ← SIMD/Rayon/Canvas, тесты менеджеров, tray_service/cover_service

- **Текущий шаг (current_step):** C5 (Блок B + C завершены)
- **Следующий ход:** Вернуться к R1 (ROADMAP-update) и выполнить закрытие фазы по Шагу 5: обновить ROADMAP.md, законсервировать _STATE_.md (done), закоммитить, вывести финальный отчёт
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress