<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** AM1.0-8.2 — С2. Новый формат блоков и колбэк
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - ROADMAP.md
  - _STATE_.yaml
  - _STATE_.md
  - src/audio/session.rs
  - src/audio/render/mod.rs
  - src/audio/render/out.rs
  - src/audio/render/tpdf.rs
  - src/audio/render/gain.rs
  - src/audio/render/pcm.rs
  - src/audio/render/dop.rs
  - src/audio/mod.rs
  - src/audio/worker.rs
  - src/audio/decoder.rs
  - src/audio/dsd.rs
  - src/audio/player.rs
  - src/audio/output.rs
  - src/app/fulltrack_manager.rs
  - tests/rt_zero_alloc.rs
  - tests/dsd512_playback_cpu_budget.rs
- **Критерий успеха (Definition of Done):** Тракт PCM/DoP работает через SampleBlock → типизированный ring (ExactI32/F32) → PcmRender/DopRender поверх текущего cpal-потока (build_output_stream_raw); удалены next_frames -> &[f32], RingBuffer<f32>, цикл слива ring, ×32767, rt_*-тесты по §7.4; тесты §7.2 этапа С2 зелёные; cargo build/test/clippy без новых варнингов; tools/check_rt_imports.py чист; плеер играет в Совместимом режиме.

## Итерационный трекер
[x] Шаг 1: SessionShared — атомики сессии (§2.9, ТЗ-98, ТЗ-99, ТЗ-100) в src/audio/session.rs. Проверка: cargo check; cargo test session:: зелёный
[x] Шаг 2: Каркас src/audio/render/mod.rs: deny-линты §7.7, RingSample для i32/f32, хелперы фазы 3 seek и priming (§6.14, §6.16, ТЗ-99). Проверка: cargo check; cargo clippy без варнингов по render/
[x] Шаг 3: OutFormat-писатели S16/S24_3LE/S24_LE/S32/F32 + quantize (§6.14 таблица усиления, ТЗ-4, ТЗ-5, ТЗ-6) в render/out.rs. Проверка: cargo test render::out (zero_pad_layout_per_format, s24le_is_right_aligned_with_sign_extension, quantize_f32_symmetric_roundtrip)
[x] Шаг 4: TPDF xorshift32 с фиксируемым seed (ТЗ-8, §6.14) в render/tpdf.rs. Проверка: cargo test render::tpdf
[x] Шаг 5: GainStage: NoGain/AtomicGain (§2.9, ОВС-18, ТЗ-7) в render/gain.rs. Проверка: cargo test render::gain
[x] Шаг 6: PcmRender<P, O, G>: фаза 3 seek, priming, underrun, eof→ended, тишина при паузе (§6.14, §6.16, §6.17, ТЗ-9, ТЗ-10, ТЗ-99, ТЗ-100, ТЗ-138) в render/pcm.rs. Проверка: cargo test render::pcm (passthrough_*, optimal_unity_gain_*, dither_*, underrun_*, priming_ends_at_eof_short_track, strict_mute_*)
[x] Шаг 7: DopRender<O>: непрерывные маркеры, payload 0x6969 при тишине, расход ring при muted (§6.15, ТЗ-98) в render/dop.rs. Проверка: cargo test render::dop (priming_dop_emits_marked_silence, dop_markers_continuous_*, dop_silence_payload_is_6969)
[ ] Шаг 8: Decoder::next_block — ExactI32 по разрядности + lossy-округление с lossy_clipped, F32 для float-PCM (§6.10, ТЗ-4, ТЗ-5) в decoder.rs. Проверка: cargo test decoder::
[ ] Шаг 9: DsdDecoder::next_block — F32 для CIC, ExactI32 с payload в битах 23..8 для DoP (§6.10, §6.15) в dsd.rs. Проверка: cargo test dsd::
[ ] Шаг 10: Перевод внешних потребителей на next_block (fulltrack_manager.rs, tests/dsd512_playback_cpu_budget.rs) и удаление next_frames из AudioSource (§8 С2 «удаляется»). Проверка: cargo check --all-targets
[ ] Шаг 11: Воркер: типизированный ring, фаза 2 seek, PendingTail, eof_frame (§6.16, §6.17, ТЗ-99, ТЗ-100) в worker.rs. Проверка: cargo test worker:: (seek_protocol_rejects_two_phase_race, seek_protocol_series_acks_only_latest)
[ ] Шаг 12: output.rs: build_output_stream_raw + bytes_mut с маппингом I16→S16, I24→S24_LE, I32→S32, F32→F32; probe_output на SessionShared (§6.14). Проверка: cargo check
[ ] Шаг 13: Player: фаза 1 seek, начальное заполнение min(50%, 300 мс), bit_perfect→NoGain, dither фиксируется при сборке, переоткрытие при переключении (§6.16, §6.18, ОВС-18, ТЗ-138) в player.rs. Проверка: cargo check; cargo run — играет в Совместимом режиме (ручная)
[ ] Шаг 14: Удаление старых колбэков и rt_*-тестов по §7.4 + сквозные тесты §7.2 (pause_resume_100x_counter_stream_is_continuous, seek_1000x_first_sample_is_target, no_false_underrun_after_seek_and_start, real_starvation_after_priming_counts) в player.rs. Проверка: cargo test audio::
[ ] Шаг 15: tests/rt_zero_alloc.rs: callback_zero_alloc_all_formats_and_states для новых рендеров (§7.2, ТЗ-10). Проверка: cargo test --test rt_zero_alloc
[ ] Шаг 16: Финальная верификация: cargo build/test/clippy, python tools/check_rt_imports.py, traceability_tool (§7.7, §8 общие условия). Проверка: всё зелёное

- **Текущий шаг (current_step):** Шаг 8
- **Следующий ход:** decoder.rs: Decoder::next_block — ring-блок ExactI32 (целые источники, valid_bits) или F32 (lossy/float), счётчик lossy_clipped (§6.10, ADR-03, ТЗ-4…ТЗ-6)
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

[x] 1. _STATE_.md → schema/YAML + generated Markdown — 436a681
[x] 2. Автоматическая проверка STATE ↔ Git whitelist — 3f0cfaf
[x] 3. Автоматическая traceability-проверка SPEC ↔ ROADMAP — 1fefed3
[x] 4. Verification scripts для performance requirements (≤5% CPU, ≤50 ms, 0 allocations, DSD512 no OOM) — 06db033, 89ce5b8, 6a4f747; провал DSD512 CPU-бюджета вынесен в ROADMAP V5.1-11.1
[~] 5. Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче — инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
[x] 6. Убрать ссылки из docs/ на запрещённый _DRAFTS_ — ссылки удалены; traceability_tool.py check #6 не даёт вернуть
[x] 7. Сделать ROADMAP автоматически валидируемым — 1fefed3 (префиксы, якоря, коммиты) + проверки #7-10 traceability_tool: форма таблиц, уникальные ID, словарь статусов/приоритетов, ссылки из _STATE_ и acceptance known_issue
[-] 8. Согласовать единую спеку аудио-тракта docs/_canceled/spec_audio_pipeline_v5.0.md (AP5.0) — Отменено 2026-09-25: AP5.0 не утверждалась и заменена docs/01_audio_modes_v1.0/ (ревью → ТЗ → спецификация, утверждены 2026-09-25). Спека и её исходники — в архиве docs/_canceled/ (не источник требований).
[>] 9. Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА — Закрыты SP1.0-8.0 (С0, f3a4c55) и SP1.0-8.1 (С1 «Модуль ФС и журнал», 479bd2e). Закрыт AM1.0-8.0 (С0 «Основа приёмки», f42a7d9). Закрыт AM1.0-8.1 (С1 (01) «P0 в текущем тракте и резервирование», 27a43b9): DoP только в Exclusive, маркеры/0x6969, clamp, лимиты DSF/DFF + фаззинг, clippy deny unwrap/expect/unreachable, бейдж ТЗ-52, ReserveDevice1 (zbus) + ExclusiveGate в Player с опросом на тике; ручные сценарии ТЗ-1/2/48/118/119/120/122 — за пользователем; отклонения старого пути до С6: повтор EBUSY на тике 100 мс, select_output_for может кратко пробовать hw: до резервирования. Следующий — AM1.0-8.2 (С2 (01) «Новый формат блоков и колбэк»). Баг AM1.0-B1 (устройство не возвращается в PipeWire после hw:, EBUSY при пересоздании узла) — проверить и исправить на С6. Отложено из С1: MemStore::delay_write/next_wake и ManualClock — в С4; сборка на Windows/macOS проверена только ревью (rustup нет) — проверить cargo build на Windows-ноутбуке; ручной strace ТЗ-19 — в С12. Старт этапа — AGENTS.md Шаги 2–3: прочитать строку этапа §8 и все относящиеся разделы (ТЗ-N, ADR-N, §2–§3, §5, §6, §7).

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
