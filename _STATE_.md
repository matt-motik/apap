<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** SP1.0-8.0 — С0. Изоляция тестов (ТЗ-49, 51 (`bp_report_marks_volume_issue`))
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/persist/mod.rs
  - src/lib.rs
  - src/settings.rs
  - src/main.rs
  - src/app/mod.rs
  - src/app/playlist_manager.rs
  - src/app/bp_report.rs
- **Критерий успеха (Definition of Done):** cargo build/test/clippy зелёные (0 новых варнингов в изменённых файлах); config_dir() вызывается только в src/main.rs; ни один тест не создаёт MusicApp; плеер запускается, играет, настройки сохраняются как раньше. Ручная проверка ТЗ-49 — пользователь.

## Итерационный трекер
[x] Шаг 1: ConfigPaths (dir, settings, state, playlist, journal; in_dir) в src/persist/mod.rs + pub mod persist в lib.rs (§2.2, ADR-19, ТЗ-49). Проверка: cargo check; cargo test persist:: зелёный
[x] Шаг 2: SettingsStore::load_from(path) в src/settings.rs; load() пока делегирует (ADR-19). Проверка: cargo check
[x] Шаг 3: main.rs: config_dir() → ConfigPaths, создание файлов тем; MusicApp::new(ui, paths) хранит пути и грузит настройки через load_from (ADR-19, ADR-23). Файлы: src/main.rs, src/app/mod.rs. Проверка: cargo check
[ ] Шаг 4: Оставшиеся config_dir()/playlist_path() в src/app/mod.rs и src/app/playlist_manager.rs → self.paths (ТЗ-49). Проверка: cargo check; grep config_dir src/app пусто
[ ] Шаг 5: Удалить SettingsStore::load() и playlist_path() из src/settings.rs (ADR-19). Проверка: cargo check
[ ] Шаг 6: build_bp_report по данным (BpInputs), тест bp_report_marks_volume_issue без MusicApp::new (ТЗ-51, ТЗ-49). Файлы: src/app/bp_report.rs, src/app/mod.rs. Проверка: cargo test bp_report зелёный
[ ] Шаг 7: Финальная верификация: cargo test, cargo clippy (фильтр по вайтлисту), grep 'config_dir()' только в src/main.rs и определении. Проверка: всё зелёное

- **Текущий шаг (current_step):** Шаг 4
- **Следующий ход:** Заменить config_dir()/playlist_path() в src/app/mod.rs и playlist_manager.rs на self.paths. Тест bp_report не собирается до шага 6 (старая сигнатура MusicApp::new) — ожидаемо
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
[>] 9. Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА — Активный этап: С0 задачи 02 (SP1.0-8.0) «Изоляция тестов», микро-сессия не начата; порядок — раздел „Сквозной порядок“ ROADMAP.md. Старт этапа — AGENTS.md Шаги 2–3: прочитать строку этапа §8 и все относящиеся разделы (ТЗ-N, ADR-N, §2–§3, §5, §6, §7).

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
