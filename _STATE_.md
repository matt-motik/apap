<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** AM1.0-8.3 — С3. Поток движка apap-engine (мост: Player внутри движка)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/lib.rs
  - src/engine/mod.rs
  - src/engine/messages.rs
  - src/engine/sink.rs
  - src/engine/spawner.rs
  - src/engine/source.rs
  - src/engine/deps.rs
  - src/engine/run.rs
  - src/engine/tests.rs
  - src/audio/error.rs
  - src/settings.rs
  - src/audio/worker.rs
  - src/audio/player.rs
  - src/audio/decoder.rs
  - src/audio/backend/mod.rs
  - src/audio/backend/shared.rs
  - src/audio/backend/catalog.rs
  - src/audio/output.rs
  - src/platform/mod.rs
  - src/platform/devwatch.rs
  - src/app/mod.rs
  - src/app/ui_audio_state.rs
  - src/app/engine_sink.rs
  - src/app/audio_facade.rs
  - src/app/playback_manager.rs
  - src/app/visualizer_manager.rs
  - src/app/playlist_manager.rs
  - src/app/bp_report.rs
  - src/main.rs
  - src/core/mod.rs
  - src/core/testing.rs
  - ROADMAP.md
  - docs/01_audio_modes_v1.0/03_spec.md
- **Критерий успеха (Definition of Done):** UI не держит Player и не делает блокирующего I/O звука (ТЗ-103/104); EngineCmd/EngineEvent через apap-engine; Rc-цикл убран; выход через Shutdown, запись settings/state не ждёт ShutdownComplete; cargo test + clippy зелёные; плеер играет в Совместимом режиме

## Итерационный трекер
[x] Шаг 1: Типы команд/событий движка: EngineCmd/EngineEvent (подмножество С3: Open{req_gen,path,start_secs,autoplay}, Play, Pause, Stop, Seek, SetVolume, SetMuted, SetDevice, RefreshDevices, SetVizTap, SetLegacyAudio, Shutdown; события Opened, OpenFailed, Skipped, Ended, Transport, Position, Devices, DeviceLost, Notice, ShutdownComplete), только варианты, которые С3 обрабатывает (§2.8, ADR-01). Файлы: src/engine/mod.rs (новый, объявление модулей), src/engine/messages.rs (новый), src/lib.rs. Проверка: cargo check
[x] Шаг 2: EventSink + VecSink (фейк, cfg(test)/testing) (§2.8, ADR-02, ADR-20). Файл: src/engine/sink.rs. Проверка: cargo check
[x] Шаг 3: Классы ошибок: OpenError (подмножество С3: Capture, DeviceLost, File, Internal; Incompatible/ModeUnavailable — С4/С5), Reaction, classify, reaction(class, ModeKind, DeviceChoiceKind) по таблице ADR-14; ModeKind {Compatible, Optimal, Strict} временно в src/settings.rs (в С4 переезжает в settings/playback.rs, решение пользователя 2026-10-07); юнит-тест error_classes_distinct_reactions (ADR-14, §2.4, ТЗ-86). Выполняется ДО шага 1 (OpenError нужен событиям). Файлы: src/audio/error.rs, src/settings.rs. Проверка: cargo test error_classes
[x] Шаг 4: ЧЕКПОИНТ шагов 1–3. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[ ] Шаг 5: ThreadSpawner + StdSpawner + FailingSpawner (отказ на N-м вызове) по §2.10 (ТЗ-88, ADR-20). Файл: src/engine/spawner.rs. Проверка: cargo check
[ ] Шаг 6: DecodeWorker::spawn принимает &dyn ThreadSpawner; Player пробрасывает StdSpawner (мост) (§6.18 шаг 6, ТЗ-88). Файлы: src/audio/worker.rs, src/audio/player.rs. Проверка: cargo check
[ ] Шаг 7: SourceOpener + FakeSource (заданный SourceFormat/FileError, счётчики probe/open) по §2.10 (§6.18 шаги 3, 6). Файл: src/engine/source.rs. Проверка: cargo check
[ ] Шаг 8: SymphoniaSourceOpener: probe — заголовок без seek-индекса (новая функция probe_header в decoder.rs; DSF/DFF через dsd.rs-заголовок), open — существующий Decoder::open/DSD (§2.10, §6.18 шаг 3, ТЗ-103). Файлы: src/audio/decoder.rs, src/engine/source.rs. Проверка: cargo test probe_header
[ ] Шаг 9: ЧЕКПОИНТ шагов 5–8. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[ ] Шаг 10: SharedBackend (§2.3, ADR-07) — перечисление и устройство по умолчанию; CpalSharedBackend поверх существующих AudioHost/CpalHost (не переписывать). Файлы: src/audio/backend/shared.rs (новый), src/audio/backend/mod.rs. Проверка: cargo check
[ ] Шаг 11: DeviceCatalog + трейт DeviceWatcher + FakeDeviceWatcher (§2.3, ADR-16, ТЗ-105). Файлы: src/audio/backend/catalog.rs (новый), src/audio/backend/mod.rs. Проверка: cargo check
[ ] Шаг 12: apap-devwatch: Linux — опрос /proc/asound/cards раз в 1 с, событие только при изменении; прочие ОС — DeviceWatcher без событий (ADR-16, ТЗ-105). cfg только здесь. Файлы: src/platform/devwatch.rs (новый), src/platform/mod.rs. Проверка: cargo test devwatch
[ ] Шаг 13: ЧЕКПОИНТ шагов 10–12. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[ ] Шаг 14: EngineDeps (подмножество С3: shared, watcher, sources: Arc, clock, store, events, spawner: Arc; reservation/exclusive/probes — на С6/С7 у Player) (ADR-20, §2.10). Файл: src/engine/deps.rs. Проверка: cargo check
[ ] Шаг 15: Цикл apap-engine (§6.1): struct Engine { player: Player (мост), deps }, recv_timeout(20 мс), транспорт Play/Pause/Stop/Seek/SetVolume/SetMuted/SetVizTap; EngineHandle {tx, JoinHandle}::spawn через ThreadSpawner, send не блокирует (ADR-01). Файл: src/engine/run.rs (новый). Проверка: cargo check
[ ] Шаг 16: Опрос сессии в цикле: Position/Transport/Ended только при изменении (И-Р13, ТЗ-60 основа); события резервирования → Notice/OpenFailed (ТЗ-102). Файл: src/engine/run.rs. Проверка: cargo check
[ ] Шаг 17: Open в движке: SourceOpener::probe → Skipped(File) с сообщением; затем Player::open (мост); отказ spawn → OpenFailed(Internal(SpawnFailed)); ошибка чтения посреди трека → Skipped/Notice, не Ended (§6.18 шаги 3, 6; ТЗ-86, ТЗ-87, ТЗ-88). Файл: src/engine/run.rs (+ src/audio/player.rs, если Player::open нужен параметр SourceOpener). Проверка: cargo check
[ ] Шаг 18: Устройства: каталог перечисляется один раз при старте движка и по событию DeviceWatcher → Devices; RefreshDevices/SetDevice; стартовая проверка устройства — в движке, вместо busy-wait probe_output (ТЗ-104, ТЗ-105). Файл: src/engine/run.rs. Проверка: cargo check
[ ] Шаг 19: SetLegacyAudio: старые параметры звука (exclusive/dsd/resampler/ring/bit_perfect/dither) применяются в памяти движка без файлового I/O — мост до ModeSettings С4 (ТЗ-134, И-Р24). Файл: src/engine/run.rs. Проверка: cargo check
[ ] Шаг 20: Shutdown (§6.28 в рамках моста): стоп вывода → стоп декодера → Player::release_engine → PersistStore::save → ShutdownComplete → выход из цикла (ТЗ-45). Файл: src/engine/run.rs. Проверка: cargo check
[ ] Шаг 21: Тесты движка на фейках: ui_handlers_do_not_block_on_slow_open (медленный FakeSource, send возвращается сразу), spawn_failure_reports_error, device_catalog_enumerated_once, device_hotplug_single_enumeration, set_mode_settings_performs_no_io (SetLegacyAudio, MemStore без записей), shutdown_emits_complete (§7, ТЗ-103/88/105/134/45). Файл: src/engine/tests.rs (новый). Проверка: cargo test engine::
[ ] Шаг 22: ЧЕКПОИНТ шагов 14–21. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[ ] Шаг 23: UiAudioState: копии «записано», запись Slint-свойства только при изменении, отброс устаревших событий по req_gen (ADR-02, И-Р13, И-Р14); юнит-тесты. Файл: src/app/ui_audio_state.rs (новый; mod в src/app/mod.rs). Проверка: cargo test ui_audio_state
[ ] Шаг 24: SlintEventSink: mpsc + флаг wake_pending + slint::invoke_from_event_loop, держит Weak (ADR-02). Файл: src/app/engine_sink.rs (новый; mod в src/app/mod.rs). Проверка: cargo check
[ ] Шаг 25: Мост AudioFacade: методы с сигнатурами Player, которые использует app (play/stop/toggle/seek/volume/muted/snapshot/is_playing/stream_desc/format/set_* …), поверх EngineHandle.send + UiAudioState; open — асинхронный (без возврата TrackInfo). Файл: src/app/audio_facade.rs (новый). Проверка: cargo check
[ ] Шаг 26: MusicApp: поле player: Player → AudioFacade, запуск движка в init, разбор событий движка на пробуждении/тике; open в playback_manager — отправка Open, ветка успеха → обработчик Opened, ошибки → OpenFailed/Skipped (ADR-01, ТЗ-103). Файлы: src/app/mod.rs, src/app/playback_manager.rs. Проверка: cargo check
[ ] Шаг 27: Rc-цикл R-20: MusicApp хранит Weak<AppWindow> (upgrade в местах использования) (ADR-01, ТЗ-45). Файлы: src/app/mod.rs, src/main.rs. Проверка: cargo check
[ ] Шаг 28: Удалить busy-wait probe_output и PROBE_OPEN_MS; окно показывается до проверки устройства, результат — событием движка (ТЗ-104). Файлы: src/audio/output.rs, src/app/mod.rs. Проверка: cargo check
[ ] Шаг 29: ЧЕКПОИНТ шагов 23–28 + ручной запуск cargo run за пользователем (играет в Совместимом режиме). ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[ ] Шаг 30: EngineSink в AppDeps/AppCore (spec 02 §2.12) + RecordingEngine-фейк. Файлы: src/core/mod.rs, src/core/testing.rs. Проверка: cargo check
[ ] Шаг 31: AppCore::exit: замыкание release_engine → deps.engine.send(Shutdown) + ожидание ShutdownComplete с подсроком 2 с; settings/state пишутся независимо (ОВС-16, ТЗ-136, §6.28); вызовы release_engine в close_requested/TrayCmd::Quit удаляются; обновить комментарий «мост до С5/С6». Файлы: src/core/mod.rs, src/app/mod.rs. Проверка: cargo check
[ ] Шаг 32: Тесты выхода: существующие тесты на новый путь + shutdown_writes_settings_even_on_timeout (ТЗ-136). Файл: src/core/testing.rs. Проверка: cargo test core::
[ ] Шаг 33: ЧЕКПОИНТ шагов 30–32. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[ ] Шаг 34: Убрать мост AudioFacade в playback_manager: прямые engine.send/UiAudioState. Файл: src/app/playback_manager.rs. Проверка: cargo check
[ ] Шаг 35: Убрать мост AudioFacade в visualizer_manager и playlist_manager. Файлы: src/app/visualizer_manager.rs, src/app/playlist_manager.rs. Проверка: cargo check
[ ] Шаг 36: Убрать мост AudioFacade в bp_report. Файл: src/app/bp_report.rs. Проверка: cargo check
[ ] Шаг 37: Убрать мост AudioFacade в mod.rs и удалить audio_facade.rs. Файлы: src/app/mod.rs, src/app/audio_facade.rs. Проверка: cargo check
[ ] Шаг 38: Финал: полный cargo test + clippy, tools/check_rt_imports.py, grep Player вне src/engine и src/audio, ROADMAP ✅, ручные проверки за пользователем (UI не блокируется, ошибки треков видны, Exclusive через старый путь). Файлы: ROADMAP.md, _STATE_.yaml. Проверка: всё зелёное

- **Текущий шаг (current_step):** Шаг 5
- **Следующий ход:** Шаг 5: ThreadSpawner/StdSpawner/FailingSpawner в src/engine/spawner.rs (§2.10). ЧЕКПОИНТ 4 зелёный (cargo test 533 ok, clippy 0). Решения пользователя 2026-10-07: мост SetLegacyAudio (вместо SetModeSettings/SetActiveMode до С4); ModeKind временно в src/settings.rs. Player — внутренняя деталь apap-engine (мост). Тесты — только на ЧЕКПОИНТ-шагах.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

- [~] **7.** Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче
  - Инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
- [>] **11.** Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА
  - Закрытые этапы и баги — в ROADMAP.md (✅ с хэшем); здесь только открытое.
  - ТЕКУЩИЙ: AM1.0-8.3 (С3 (01) «Поток движка») — микро-шаги в steps; потом SP1.0-8.6.
  - Ручные проверки за пользователем: AM1.0-8.1 — сценарии ТЗ-1/2/48/118/119/120/122.
  - Ручные проверки за пользователем: SP1.0-8.2 — меню при открытом диалоге/окне сообщения, Enter/Esc, Совместимый режим.
  - Ручные проверки за пользователем: SP1.0-8.3/8.4 — cargo run; закрытие окна и выход из трея сохраняют геометрию, плейлист, громкость.
  - Ручные проверки за пользователем: SP1.0-B5/B6 — «Файл → Удалить выделенный трек» / «Очистить плейлист»; при открытых «Параметрах» пункты неактивны.
  - Геометрия окна под Wayland проверена пользователем 2026-10-06; X11 не проверяется (решение пользователя).
  - Отклонения старого пути до С6: повтор EBUSY на тике 100 мс; select_output_for может кратко пробовать hw: до резервирования.
  - На С6: баг AM1.0-B1 (устройство не возвращается в PipeWire после hw:, EBUSY при пересоздании узла); тест unreadable_playlist_never_written (в AppCore нет чтения плейлиста).
  - VizCycle (viz_settings_manager.rs) защищён только Slint-оверлеем.
  - Ручные проверки за пользователем: SP1.0-8.5 — SIGTERM/SIGINT/SIGHUP (kill) сохраняют state/settings, второй сигнал завершает сразу; выход из трея; уведомление при ошибке записи с окном в трее.
  - На Windows-ноутбуке/macOS: собрать и проверить SP1.0-8.5 (WM_ENDSESSION-сабкласс windows.rs, applicationShouldTerminate: macos.rs, отсутствие трея) — локально не компилировалось.
  - Строка tray_works_during_dialog: проверка SetModeSettings и колёсика — на С11/с модулем режимов.
  - SP1.0-B5 вынес из С9 перенос кнопок плейлиста в меню; на С9 остаётся остальное. Текст ТЗ-34 «Удалить текущий трек» расходится с реализацией (выделенный) — правка docs по разрешению пользователя.
  - На Windows-ноутбуке: проверить cargo build (Windows/macOS проверены только ревью).
  - На С12: ручной strace ТЗ-19.

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
