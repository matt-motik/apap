<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** AM1.0-8.0 — С0. Основа приёмки: типы §2.1/§2.4, Clock/ManualClock, генераторы сигналов, тестовые данные, линтер, check_rt_imports (ТЗ-114 каркас, ТЗ-100, ТЗ-101)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - docs/01_audio_modes_v1.0/03_spec.md
  - ROADMAP.md
  - Cargo.toml
  - Cargo.lock
  - src/app/mod.rs
  - src/audio/mod.rs
  - src/audio/format.rs
  - src/audio/error.rs
  - src/audio/backend/mod.rs
  - src/audio/clock.rs
  - src/audio/testing/mod.rs
  - src/audio/testing/signals.rs
  - tools/gen_test_audio.py
  - tests/data/dff_bad_header.dff
  - tests/data/dff_chunk_size_overflow.dff
  - tests/data/dff_dsd64_1k.dff
  - tests/data/dff_truncated.dff
  - tests/data/dff_zero_channels.dff
  - tests/data/dsf_bad_header.dsf
  - tests/data/dsf_block_size_out_of_range.dsf
  - tests/data/dsf_chunk_size_overflow.dsf
  - tests/data/dsf_dsd64_1k.dsf
  - tests/data/dsf_dsd64_20k.dsf
  - tests/data/dsf_offset_beyond_eof.dsf
  - tests/data/dsf_truncated.dsf
  - tests/data/dsf_zero_channels.dsf
  - tests/data/flac_16_44k1_md5.flac
  - tests/data/flac_corrupt_frame.flac
  - tests/data/flac_md5_mismatch.flac
  - tests/data/flac_md5_zero.flac
  - tests/data/mp3_44k1_stereo.mp3
  - tests/data/README.md
  - tools/check_rt_imports.py
  - hooks/pre-commit
- **Критерий успеха (Definition of Done):** Линтер: до обёртки Slint 3248 предупреждений (3231 в коде Slint), после — 17 в старом коде (С1). cargo build/test/clippy зелёные; новые модули С0 с #![deny(unwrap_used, expect_used, unreachable)] без нарушений; [lints.clippy] warn для старого кода, код Slint обёрнут allow; tests/data сгенерированы скриптом; check_rt_imports в pre-commit. Решения пользователя 2026-10-02: EngineDeps по этапам (таблица под §8), Container с Adts.

## Итерационный трекер
[x] Шаг 1: Правки 03_spec.md по решению пользователя 2026-10-02 (шапка, Container в §2.1, С0/С3 в §8, таблица «Трейт → этап») + ROADMAP 🔄. Проверка: traceability_tool check
[x] Шаг 2: Линтер §7.7: [lints.clippy] unwrap_used/expect_used/unreachable = warn; slint::include_modules! в модуле с allow (ТЗ-101). Файлы: Cargo.toml, src/app/mod.rs. Проверка: cargo clippy, число предупреждений до/после
[x] Шаг 3: audio/format.rs: типы §2.1 + Container, конструкторы с проверкой, field_bits, RateFamily (ТЗ-12, ТЗ-74, ТЗ-101); smallvec прямой зависимостью. Файлы: src/audio/format.rs, src/audio/mod.rs, Cargo.toml. Проверка: cargo test audio::format
[x] Шаг 4: audio/error.rs: Incompatibility, CaptureFailure, FileError, CorruptKind, StreamFault, EngineFault, ErrorClass, DeviceChoiceKind; RateSet в audio/backend/mod.rs (§2.3, §2.4). Проверка: cargo test audio::error
[x] Шаг 5: audio/clock.rs: Clock, ClockInstant, MonotonicClock (ADR-20, ОВС-10 п. 7). Проверка: cargo test audio::clock
[x] Шаг 6: audio/testing: ManualClock (mod.rs) и генераторы signals.rs — поток-счётчик, 16-бит полный, 24-бит ТЗ-4, 32-бит ТЗ-5, синус, свип, импульс в последнем кадре (§7.1). Проверка: cargo test audio::testing
[x] Шаг 7: tools/gen_test_audio.py и tests/data/: FLAC с MD5 / изменённый байт / нулевой MD5 / повреждённый кадр, DSF 1 кГц/20 кГц, обрезанные и повреждённые DSF/DFF, MP3 (§7.1). Проверка: скрипт детерминирован, ffprobe открывает корректные файлы
[ ] Шаг 8: tools/check_rt_imports.py (§7.7, ТЗ-98, ТЗ-102) + вызов в hooks/pre-commit. Проверка: скрипт на audio/render/** (пока нет) — OK; на тестовом нарушении — ошибка
[ ] Шаг 9: Финальная верификация: cargo build/test/clippy; отчёт о числе предупреждений старого кода. Проверка: всё зелёное

- **Текущий шаг (current_step):** Шаг 8
- **Следующий ход:** tools/check_rt_imports.py + pre-commit
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
[>] 9. Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА — Закрыты SP1.0-8.0 (С0, f3a4c55) и SP1.0-8.1 (С1 «Модуль ФС и журнал», 479bd2e). В работе AM1.0-8.0 (С0 «Основа приёмки»), микро-шаги — в steps. Отложено из С1: MemStore::delay_write/next_wake и ManualClock — в С4; сборка на Windows/macOS проверена только ревью (rustup нет) — проверить cargo build на Windows-ноутбуке; ручной strace ТЗ-19 — в С12. Старт этапа — AGENTS.md Шаги 2–3: прочитать строку этапа §8 и все относящиеся разделы (ТЗ-N, ADR-N, §2–§3, §5, §6, §7).

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
