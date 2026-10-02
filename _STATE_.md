<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** AM1.0-8.1 — С1. P0 в текущем тракте и резервирование (ТЗ-1, 2, 3, 17, 92, 100, 101, 52; 48, 118–120, 122 в объёме старого пути)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - ROADMAP.md
  - Cargo.toml
  - Cargo.lock
  - build.rs
  - src/main.rs
  - src/app/mod.rs
  - src/app/bp_report.rs
  - src/app/playback_manager.rs
  - src/app/viz_settings_manager.rs
  - src/audio/mod.rs
  - src/audio/dop.rs
  - src/audio/dsd.rs
  - src/audio/player.rs
  - src/audio/worker.rs
  - src/audio/output.rs
  - src/audio/fulltrack.rs
  - src/audio/reservation/mod.rs
  - src/audio/reservation/gate.rs
  - src/audio/reservation/dbus.rs
  - src/audio/testing/mod.rs
  - tests/parsers_survive_mutations.rs
  - ui/status.slint
  - ui/app.slint
- **Критерий успеха (Definition of Done):** cargo build/test/clippy зелёные, clippy unwrap/expect/unreachable = deny без нарушений; DoP только в Exclusive, маркеры непрерывны, тишина 0x6969; DSF/DFF без паник (фаззинг 10 000); бейдж не зелёный; hw: только после резервирования. Отклонения старого пути (до С6): повтор EBUSY — на тике 100 мс вместо 50 мс; ожидание резервирования — опрос на тике; ExclusiveMode::Auto больше не откатывается в Shared.

## Итерационный трекер
[ ] Шаг 1: DoPFramer::reset и сброс упаковщика DoP при seek в DsdDecoder (R-24, ADR-12, ТЗ-2). Файлы: src/audio/dop.rs, src/audio/dsd.rs. Проверка: cargo test dop
[ ] Шаг 2: DoP-колбэк старого пути: маркер по собственному счётчику фазы колбэка, нагрузка 0x6969 в паузе/seek/underrun (ADR-12 Б, ТЗ-2, ТЗ-3). Файлы: src/audio/player.rs, src/audio/worker.rs. Проверка: cargo test dop_markers_continuous_through_pause_seek_underrun dop_silence_payload_is_6969
[ ] Шаг 3: DoP только в Exclusive в старом пути: open_dop отклоняет Shared, Exclusive-поток без отката в Shared (ТЗ-1). Файл: src/audio/player.rs. Проверка: cargo test old_path_dop_requires_exclusive
[ ] Шаг 4: ТЗ-17: clamp в текущем квантовании колбэков i16/u8/i32 (без wrap-around). Файл: src/audio/player.rs. Проверка: cargo test no_wraparound
[ ] Шаг 5: Лимиты разбора DSF/DFF §6.29 (channels 1..=8, block 1..=65536, checked-арифметика, offset ≤ file_len, буферы по min(заявлено, file_len)) (ТЗ-92). Файл: src/audio/dsd.rs. Проверка: cargo test dsf_corrupt_headers_error_not_panic dff_chunk_size_overflow_error truncated_files_error
[ ] Шаг 6: Фаззинг разборщиков: tests/parsers_survive_mutations.rs, 10 000 итераций, счётчик аллокаций ≤ file_len + 16 МиБ, APAP_FUZZ_ITERS только увеличивает (ТЗ-92, И-Р15). Проверка: cargo test --test parsers_survive_mutations
[ ] Шаг 7: Удалить unwrap/expect/unreachable из прод-кода библиотеки: dsd.rs, fulltrack.rs, output.rs (ТЗ-101, §6.29). Проверка: cargo clippy без предупреждений в этих файлах
[ ] Шаг 8: Удалить unwrap/expect из бинарника и build.rs: viz_settings_manager.rs, app/mod.rs, main.rs, build.rs; [lints.clippy] → deny (ТЗ-101, §6.29). Проверка: cargo clippy 0 предупреждений
[ ] Шаг 9: Бейдж старого пути: bp-active всегда false, текст «Не bit-perfect: проверка недоступна» (ТЗ-52, §8 С1). Файлы: src/app/bp_report.rs, ui/status.slint (+ app.slint привязка). Проверка: cargo test bp_report
[ ] Шаг 10: Резервирование: ReservationService, Reservation, ReservationGuard, AudioServerProbe, ReservationMsg, трейт ReserveBus и acquire() по §6.6 (до OPEN); FakeReserveBus/FakeServerProbe (ADR-08, ТЗ-48, 118, 119; ОВ-33..35). Файлы: src/audio/reservation/mod.rs, src/audio/testing/mod.rs. Проверка: cargo test reservation
[ ] Шаг 11: ExclusiveGate старого пути: Pending → Held → открытие с повтором EBUSY ≤ 1 с → BusyOutsideProtocol; освобождение PCM → ReleaseName; NameLost → закрыть PCM (И-Р1, И-Р20, И-Р21, ТЗ-122). Файл: src/audio/reservation/gate.rs. Проверка: cargo test reservation_before_pcm_order ebusy name_lost
[ ] Шаг 12: DbusReservation (zbus blocking, cfg linux), реальный AudioServerProbe, номер карты по /proc/asound; zbus прямой зависимостью Linux (ADR-08, §6.6). Файлы: src/audio/reservation/dbus.rs, Cargo.toml. Проверка: cargo check; ручной сценарий busctl
[ ] Шаг 13: Player: exclusive-открытие только через ExclusiveGate (ожидание резервирования не блокирует; отказ — ошибка без Shared). Файл: src/audio/player.rs. Проверка: cargo test audio::player
[ ] Шаг 14: Приложение: опрос резервирования на тике 100 мс, продолжение воспроизведения после Held, ошибка захвата и NameLost — стоп и сообщение. Файлы: src/app/playback_manager.rs, src/app/mod.rs. Проверка: cargo build; запуск
[ ] Шаг 15: Финальная верификация: cargo build/test/clippy; ручные сценарии ТЗ-1/2/48/118/119/120/122 — пользователю. Проверка: всё зелёное

- **Текущий шаг (current_step):** Шаг 1
- **Следующий ход:** DoPFramer::reset и вызов в DsdDecoder::seek
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
[>] 9. Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА — Закрыты SP1.0-8.0 (С0, f3a4c55) и SP1.0-8.1 (С1 «Модуль ФС и журнал», 479bd2e). Закрыт AM1.0-8.0 (С0 «Основа приёмки», f42a7d9). В работе AM1.0-8.1 (С1 (01) «P0 в текущем тракте и резервирование»), микро-шаги — в steps; в нём же — 17 оставшихся предупреждений unwrap/expect/unreachable старого кода и ReservationService/AudioServerProbe с фейками (таблица «Трейт → этап» §8). Отложено из С1: MemStore::delay_write/next_wake и ManualClock — в С4; сборка на Windows/macOS проверена только ревью (rustup нет) — проверить cargo build на Windows-ноутбуке; ручной strace ТЗ-19 — в С12. Старт этапа — AGENTS.md Шаги 2–3: прочитать строку этапа §8 и все относящиеся разделы (ТЗ-N, ADR-N, §2–§3, §5, §6, §7).

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
