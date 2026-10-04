<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** SP1.0-8.3 — С3 (02). Два файла и разбор по ключам (ТЗ-1, 2, 4, 5, 6, 7, 8, 9, 33 (параметр), ОВС-6)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - ROADMAP.md
  - _STATE_.yaml
  - _STATE_.md
  - src/persist/mod.rs
  - src/persist/keys.rs
  - src/persist/settings_file.rs
  - src/persist/state_file.rs
  - src/core/mod.rs
  - src/core/testing.rs
  - src/settings.rs
  - src/main.rs
  - src/lib.rs
  - src/cover.rs
  - src/playlist_layout.rs
  - src/audio/visualizer.rs
  - src/app/mod.rs
  - src/app/ui_manager.rs
  - src/app/playback_manager.rs
  - src/app/playlist_manager.rs
  - src/app/viz_settings_manager.rs
  - src/app/fulltrack_manager.rs
  - src/app/visualizer_manager.rs
  - src/app/bp_report.rs
  - ui/settings.slint
  - ui/app.slint
- **Критерий успеха (Definition of Done):** Два файла settings.toml/state.toml разбираются по ключам (§2.3–§2.6, §6.1–§6.2); повреждённый файл не стирает настроек (.bad до первой записи); запуск ничего не пишет; SettingsStore/migrate_legacy_columns/unwrap_or_default удалены; тесты §7.2 С3 и миграции §7.5 зелёные; cargo build/test/clippy зелёные

## Итерационный трекер
[x] Шаг 1: persist/keys.rs: KeyPath, LoadNote(Kind), KeySpec, FileRead, Parsed, walk (lookup по точкам, leaf_paths → Unknown, сортировка заметок) + юнит-тесты walk (§2.3, §6.2, ТЗ-5). Проверка: cargo test persist::keys
[x] Шаг 2: persist/settings_file.rs: типы новой Settings с дефолтами (ThemeName, SaveInterval, TopPanelLayout, ColumnsConfig/ColumnDef/ColumnKind, CoverSettings, InfoLabelKey, VizSettings, legacy audio/dsd/audio_device); Ord для ColumnId в settings.rs (§2.4, ТЗ-1, ТЗ-33). Проверка: cargo check
[x] Шаг 3: settings_file.rs: перенос методов-помощников (ordered_columns, move_column, cover-помощники, info labels) с сохраняемыми тестами §7.5 (§2.4). Проверка: cargo test persist::settings_file
[x] Шаг 4: settings_file.rs: таблица SETTINGS KeySpec + parse_settings (Adjusted для column_order/cover_priority, legacy audio-ключи) + тесты parse_column_order_adjusts, cover_priority, legacy_audio_keys_parse_by_keys, empty_files_give_documented_defaults (часть settings), save_interval_invalid_value (§2.4, §6.2, ТЗ-4, ТЗ-5, ТЗ-33, НФ-1). Проверка: cargo test persist::settings_file
[x] Шаг 5: settings_file.rs: serialize_settings через DTO SettingsFile (фиксированный порядок, BTreeMap) + settings_roundtrip_deterministic (§2.6, НФ-3). Проверка: cargo test settings_roundtrip
[x] Шаг 6: persist/state_file.rs: SessionState (legacy volume 0..100/muted), WidthPct, WindowGeometry, SortKey, Origin, StateChange, таблица STATE + parse_state + тесты parse_by_keys_three_notes и пары sort (§2.5, §6.2, ТЗ-2, ТЗ-5). Проверка: cargo test persist::state_file
[ ] Шаг 7: state_file.rs: serialize_state через DTO StateFile + state_roundtrip_deterministic + settings_and_state_keys_disjoint_and_cover_lists (§2.6, ТЗ-1, ТЗ-2). Проверка: cargo test persist::
[ ] Шаг 8: persist/mod.rs: ReferenceText, read_config(reader, path) → FileRead, Boot/boot(reader, paths), write_bad_copies (§2.2, §6.1, ADR-23 шаги 0–2, И-Р12, И-Р18, ТЗ-6, ТЗ-7). Проверка: cargo test persist::
[ ] Шаг 9: core/mod.rs: AppCore владеет Settings/SessionState/эталонами, change_state(Origin, StateChange), set_settings, синхронный flush с сравнением с эталоном, forbid_auto, стартовые сообщения/журнал из Boot (§6.1, И-Р3, И-Р20, И-Т7, ОВС-6, ТЗ-8, ТЗ-9). Проверка: cargo check
[ ] Шаг 10: core/testing.rs: Harness на MemStore + тесты С3 §7.2 (startup/exit/unparsable/unreadable/legacy/external_change) (§7.1, §7.2). Проверка: cargo test core::testing
[ ] Шаг 11: main.rs: boot до окна, .bad-копии через FileWriter, передача AppCore в MusicApp::new (ADR-23 шаги 0–2, ТЗ-6). Проверка: cargo check
[ ] Шаг 12: Атомарный перевод MusicApp с SettingsStore на AppCore (src/app/*, cover.rs, bp_report.rs, playlist_layout.rs): настройки — settings(), состояние — change_state; запись целиком в старые моменты (§8.1 С3). Обоснованное исключение из правила 1–2 файлов: смена типа требует атомарной компиляции. Проверка: cargo build + cargo test
[ ] Шаг 13: ui/settings.slint + app.slint: параметр N (10/30/60/120) с подсказкой §2.13, сохранение значения по «Сохранить» (ТЗ-33 параметр). Проверка: cargo build
[ ] Шаг 14: ui/settings.slint: постоянный текст ОВС-6 рядом с «Сохранить» при нечитаемом settings.toml (§2.13, ОВС-6, И-Р20). Проверка: cargo build
[ ] Шаг 15: settings.rs: удалить SettingsStore, старую Settings, migrate_legacy_columns и устаревшие тесты по §7.5 (§8.1 «что удаляется»). Проверка: cargo test
[ ] Шаг 16: Финальная верификация: cargo build/test/clippy зелёные, закрытие этапа (Шаг 5). Проверка: все зелёные, 0 новых варнингов

- **Текущий шаг (current_step):** Шаг 7
- **Следующий ход:** Шаг 7: serialize_state через DTO StateFile + roundtrip + disjoint keys
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
[>] 9. Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА — Закрыты SP1.0-8.0 (С0, f3a4c55) и SP1.0-8.1 (С1 «Модуль ФС и журнал», 479bd2e). Закрыт AM1.0-8.0 (С0 «Основа приёмки», f42a7d9). Закрыт AM1.0-8.1 (С1 (01) «P0 в текущем тракте и резервирование», 27a43b9): DoP только в Exclusive, маркеры/0x6969, clamp, лимиты DSF/DFF + фаззинг, clippy deny unwrap/expect/unreachable, бейдж ТЗ-52, ReserveDevice1 (zbus) + ExclusiveGate в Player с опросом на тике; ручные сценарии ТЗ-1/2/48/118/119/120/122 — за пользователем; отклонения старого пути до С6: повтор EBUSY на тике 100 мс, select_output_for может кратко пробовать hw: до резервирования. Закрыт AM1.0-8.2 (С2 (01) «Новый формат блоков и колбэк», 1672b9e): SampleBlock/ExactI32, типизированный ring i32/f32, SessionShared + RenderCore (трёхфазный seek, priming, underrun), PcmRender/DopRender над bytes_mut (I16/I24/I32/F32), TPDF фиксирован при сборке, NoGain в bit-perfect, decode loop с PendingTail/eof_frame, старый f32-тракт удалён; e2e-тесты §7.2 и zero-alloc по всем форматам/состояниям; F32-выход ограничен [−1,1] (ТЗ-17). Закрыт SP1.0-8.2 (С2 (02) «Окно сообщений и шлюз», e0ffd7d): core::gate UiGate/MainCmd (ADR-12), core::messages MessageCenter (ADR-13, §6.15, §6.8), PlatformCaps/Notification-заготовки, ui/message_window.slint + оверлей window-blocked, allows() во всех обработчиках главного окна (трей мимо), ошибки записи settings/playlist → сводное окно с «Повторить», раскладка ТЗ-52 в playback/playlist; notify до С5 идёт подсказкой трея; VizCycle (viz_settings_manager.rs) защищён только Slint-оверлеем; ручные проверки (меню при диалоге/окне, Enter/Esc, Совместимый режим) — за пользователем; баг SP1.0-B1 (молчаливая неудача загрузки плейлиста) — в ROADMAP. Следующий по сквозному порядку — SP1.0-8.3 (С3 (02) «Два файла и разбор по ключам»), затем SP1.0-8.4…8.5, потом AM1.0-8.3. Баг AM1.0-B1 (устройство не возвращается в PipeWire после hw:, EBUSY при пересоздании узла) — проверить и исправить на С6. Отложено из С1: MemStore::delay_write/next_wake и ManualClock — в С4; сборка на Windows/macOS проверена только ревью (rustup нет) — проверить cargo build на Windows-ноутбуке; ручной strace ТЗ-19 — в С12. Старт этапа — AGENTS.md Шаги 2–3: прочитать строку этапа §8 и все относящиеся разделы (ТЗ-N, ADR-N, §2–§3, §5, §6, §7).

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
