<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** SP1.0-8.1 — С1. Модуль ФС и журнал (ТЗ-18, 19, 20 (классы, журнал), 54 п. 2)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - ROADMAP.md
  - Cargo.toml
  - Cargo.lock
  - src/lib.rs
  - src/platform/mod.rs
  - src/platform/fs/mod.rs
  - src/platform/fs/linux.rs
  - src/platform/fs/macos.rs
  - src/platform/fs/windows.rs
  - src/platform/fs/mem.rs
  - src/platform/fs/engine_store.rs
  - src/persist/mod.rs
  - src/journal.rs
  - src/settings.rs
  - src/playlist.rs
  - src/main.rs
  - src/app/mod.rs
  - src/app/ui_manager.rs
  - src/app/viz_settings_manager.rs
  - src/app/playback_manager.rs
  - src/app/playlist_manager.rs
- **Критерий успеха (Definition of Done):** cargo build/test/clippy зелёные (0 новых варнингов в изменённых файлах); settings.toml и playlist.m3u пишутся через FileWriter (ADR-4: tmp → sync → rename → sync каталога), ошибки — в Journal (apap.log + stderr), fs::write/eprintln этих путей удалены; cfg(target_os) только в src/platform/. Ручные проверки — strace (ТЗ-19, Linux), сборка на Windows — пользователь.

## Итерационный трекер
[x] Шаг 1: platform/fs/mod.rs: ReadError, WriteError, классы, WriteStep, classify по таблицам кодов ОС, трейты FileReader/FileWriter; pub mod platform (ADR-4, ADR-6, §2.8, ТЗ-20, ТЗ-54). Файлы: src/lib.rs, src/platform/mod.rs, src/platform/fs/mod.rs. Проверка: cargo test classify_os_errors
[x] Шаг 2: WorkFile, ConfigFile в persist/mod.rs (§2.2). Проверка: cargo check
[x] Шаг 3: journal.rs: Journal, JournalRecord (варианты С1), WriteTarget, VecJournal, формат строки (ADR-21, §2.9). Файлы: src/journal.rs, src/lib.rs. Проверка: cargo test journal::
[x] Шаг 4: OsFs: последовательность §6.7 + примитивы Linux (linux.rs), os_fs() (ADR-4, ТЗ-18, ТЗ-19). Файлы: src/platform/fs/mod.rs, src/platform/fs/linux.rs, Cargo.toml. Проверка: cargo test platform::fs (запись во временный каталог)
[x] Шаг 5: Примитивы macOS: F_FULLFSYNC → fsync для файла и каталога (ADR-4). Файл: src/platform/fs/macos.rs. Проверка: cargo check; ревью против таблицы ADR-4
[ ] Шаг 6: Примитивы Windows: MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH), без sync каталога (ADR-4). Файлы: src/platform/fs/windows.rs, Cargo.toml. Проверка: cargo check; ревью против таблицы ADR-4
[ ] Шаг 7: MemStore: файлы в памяти, шаги ADR-4, fail_write/crash, fail_read, counts, calls (§2.8, §6.7, ADR-19). Файл: src/platform/fs/mem.rs. Проверка: cargo test atomic_write_interrupted_at_each_step
[ ] Шаг 8: PersistStore, EngineFile, FsPersistStore (§2.8, ADR-19). Файл: src/platform/fs/engine_store.rs. Проверка: cargo test engine_store
[ ] Шаг 9: FileJournal: поток apap-log, ротация 3 файлов, лимит 1 МиБ, отключение при ошибке, flush, время civil_from_days (ADR-21, §6.16, ОВС-4 б). Файл: src/journal.rs. Проверка: cargo test file_journal
[ ] Шаг 10: main.rs: os_fs() + FileJournal::start(paths.journal); MusicApp хранит FileWriter рабочих файлов и Journal (ADR-19). Файлы: src/main.rs, src/app/mod.rs. Проверка: cargo check
[ ] Шаг 11: SettingsStore::save через FileWriter → Result; ошибка — JournalRecord::WriteFailed; load_from без записи, запись при запуске — из MusicApp::new (прежний момент) (ТЗ-18, ТЗ-20). Файлы: src/settings.rs + механическая замена вызовов в src/app/*. Проверка: cargo test settings
[ ] Шаг 12: save_track_list через FileWriter → Result; ошибка — журнал; тест на MemStore (ТЗ-18, ТЗ-20). Файлы: src/playlist.rs, src/app/playlist_manager.rs. Проверка: cargo test playlist
[ ] Шаг 13: Финальная верификация: cargo test, cargo clippy (фильтр по вайтлисту), grep cfg(target_os вне src/platform пуст, fs::write в settings/playlist нет. Проверка: всё зелёное

- **Текущий шаг (current_step):** Шаг 6
- **Следующий ход:** Примитивы Windows: MoveFileExW (windows.rs), windows-sys в Cargo.toml
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
[>] 9. Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА — Закрыт SP1.0-8.0 (С0 «Изоляция тестов», f3a4c55). В работе SP1.0-8.1 (С1 «Модуль ФС и журнал»), микро-шаги — в steps; порядок — раздел „Сквозной порядок“ ROADMAP.md. Старт этапа — AGENTS.md Шаги 2–3: прочитать строку этапа §8 и все относящиеся разделы (ТЗ-N, ADR-N, §2–§3, §5, §6, §7).

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
