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
  - src/audio/dsd.rs
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
[x] Шаг 5: ThreadSpawner + StdSpawner + FailingSpawner (отказ на N-м вызове) по §2.10 (ТЗ-88, ADR-20). Файл: src/engine/spawner.rs. Проверка: cargo check
[x] Шаг 6: DecodeWorker::spawn принимает &dyn ThreadSpawner; Player пробрасывает StdSpawner (мост) (§6.18 шаг 6, ТЗ-88). Файлы: src/audio/worker.rs, src/audio/player.rs. Проверка: cargo check
[x] Шаг 7: SourceOpener + FakeSource (заданный SourceFormat/FileError, счётчики probe/open) по §2.10 (§6.18 шаги 3, 6). Файл: src/engine/source.rs. Проверка: cargo check
[x] Шаг 8: probe_header(path) -> Result<SourceFormat, FileError> для symphonia-форматов: только заголовок/параметры трека без декодирования и без seek-индекса; маппинг Container/Codec/lossy/SourceKind (Pcm bits, FloatPcm, lossy → 24 бит) (§2.1, §2.10, §6.18 шаг 3, ТЗ-103, ТЗ-74, ОВ-36, ОВС-1). Файл: src/audio/decoder.rs. Проверка: cargo test probe_header
[x] Шаг 9: DSD: заголовок DSF/DFF → SourceFormat (Container/Codec Dsf|Dff, rate = DSD-бит/канал, kind Dsd{DsdRate}) без открытия декодера (§2.1, §2.10, §6.18 шаг 3, ТЗ-103). Файл: src/audio/dsd.rs (вайтлист расширен решением пользователя 2026-10-07). Проверка: cargo test probe_dsd
[x] Шаг 10: SymphoniaSourceOpener: probe — DSF/DFF по расширению → шаг 9, иначе probe_header; open — DsdDecoder::open (PCM) / Decoder::open, String-ошибка → FileError (мост) (§2.10, §6.18 шаги 3, 6). Файл: src/engine/source.rs. Проверка: cargo test engine::source
[x] Шаг 11: ЧЕКПОИНТ шагов 5–10. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[x] Шаг 12: SharedBackend (§2.3, ADR-07) — перечисление и устройство по умолчанию; CpalSharedBackend поверх существующих AudioHost/CpalHost (не переписывать). Файлы: src/audio/backend/shared.rs (новый), src/audio/backend/mod.rs. Проверка: cargo check
[x] Шаг 13: DeviceCatalog + трейт DeviceWatcher + FakeDeviceWatcher (§2.3, ADR-16, ТЗ-105). Файлы: src/audio/backend/catalog.rs (новый), src/audio/backend/mod.rs. Проверка: cargo check
[x] Шаг 14: apap-devwatch: Linux — опрос /proc/asound/cards раз в 1 с, событие только при изменении; прочие ОС — DeviceWatcher без событий (ADR-16, ТЗ-105). cfg только здесь. Файлы: src/platform/devwatch.rs (новый), src/platform/mod.rs. Проверка: cargo test devwatch
[x] Шаг 15: ЧЕКПОИНТ шагов 12–14. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[x] Шаг 16: EngineDeps (подмножество С3: shared, watcher, sources: Arc, clock, store, events, spawner: Arc; reservation/exclusive/probes — на С6/С7 у Player) (ADR-20, §2.10). Файл: src/engine/deps.rs. Проверка: cargo check
[x] Шаг 17: Цикл apap-engine (§6.1): struct Engine { player: Player (мост), deps }, recv_timeout(20 мс), транспорт Play/Pause/Stop/Seek/SetVolume/SetMuted/SetVizTap; EngineHandle {tx, JoinHandle}::spawn через ThreadSpawner, send не блокирует (ADR-01). Файл: src/engine/run.rs (новый). Проверка: cargo check
[x] Шаг 18: Опрос сессии в цикле: Position/Transport/Ended только при изменении (И-Р13, ТЗ-60 основа); события резервирования → Notice/OpenFailed (ТЗ-102). Файл: src/engine/run.rs. Проверка: cargo check
[x] Шаг 19: Open в движке: SourceOpener::probe → Skipped(File) с сообщением; затем Player::open (мост); отказ spawn → OpenFailed(Internal(SpawnFailed)); ошибка чтения посреди трека → Skipped/Notice, не Ended (§6.18 шаги 3, 6; ТЗ-86, ТЗ-87, ТЗ-88). Файл: src/engine/run.rs (+ src/audio/player.rs, если Player::open нужен параметр SourceOpener). Проверка: cargo check
[x] Шаг 20: Устройства: каталог перечисляется один раз при старте движка и по событию DeviceWatcher → Devices; RefreshDevices/SetDevice; стартовая проверка устройства — в движке, вместо busy-wait probe_output (ТЗ-104, ТЗ-105). Файл: src/engine/run.rs. Проверка: cargo check
[x] Шаг 21: SetLegacyAudio: старые параметры звука (exclusive/dsd/resampler/ring/bit_perfect/dither) применяются в памяти движка без файлового I/O — мост до ModeSettings С4 (ТЗ-134, И-Р24). Файл: src/engine/run.rs. Проверка: cargo check
[x] Шаг 22: Shutdown (§6.28 в рамках моста): стоп вывода → стоп декодера → Player::release_engine → PersistStore::save → ShutdownComplete → выход из цикла (ТЗ-45). Файл: src/engine/run.rs. Проверка: cargo check
[x] Шаг 23: Тесты движка на фейках: ui_handlers_do_not_block_on_slow_open (медленный FakeSource, send возвращается сразу), spawn_failure_reports_error, device_catalog_enumerated_once, device_hotplug_single_enumeration, set_mode_settings_performs_no_io (SetLegacyAudio, MemStore без записей), shutdown_emits_complete (§7, ТЗ-103/88/105/134/45). Файл: src/engine/tests.rs (новый). Проверка: cargo test engine::
[x] Шаг 24: ЧЕКПОИНТ шагов 16–23. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[x] Шаг 25: UiAudioState: копии «записано», запись Slint-свойства только при изменении, отброс устаревших событий по req_gen (ADR-02, И-Р13, И-Р14); юнит-тесты. Файл: src/app/ui_audio_state.rs (новый; mod в src/app/mod.rs). Проверка: cargo test ui_audio_state
[x] Шаг 26: SlintEventSink: mpsc + флаг wake_pending + slint::invoke_from_event_loop, держит Weak (ADR-02). Файл: src/app/engine_sink.rs (новый; mod в src/app/mod.rs). Проверка: cargo check
[x] Шаг 27: Мост AudioFacade: методы с сигнатурами Player, которые использует app (play/stop/toggle/seek/volume/muted/snapshot/is_playing/stream_desc/format/set_* …), поверх EngineHandle.send + UiAudioState; open — асинхронный (без возврата TrackInfo). Файл: src/app/audio_facade.rs (новый). Проверка: cargo check
[x] Шаг 28: Вынести обработку результата открытия в playback_manager в отдельные методы: ветка Ok(info) → on_track_opened(index, &TrackInfo), ветка Err → on_open_failed(index, err); поведение не меняется, Player пока прежний (подготовка к асинхронному open, ADR-01, ТЗ-103). Файл: src/app/playback_manager.rs. Проверка: cargo check + cargo test --bin music-player-rs
[x] Шаг 29: Запуск движка в main.rs ДО AppCore::with_deps (ADR-19: deps строит main): event_channel(slint_wake()) + EngineDeps::system(events, dir, journal) + EngineHandle::spawn; MusicApp::new принимает Result<EngineHandle, EngineFault> и EngineEventQueue и хранит их в новых полях (Player пока прежний). Файлы: src/main.rs, src/app/mod.rs (сигнатура new + поля). Проверка: cargo check
[x] Шаг 30: MusicApp: поле player: Player → AudioFacade (EngineHandle из поля + LegacyAudio из настроек вместо блока Player::new/set_*; ошибка spawn → AudioFacade без движка + сообщение пользователю); exit: замыкание release_engine → AudioFacade::shutdown (мост до шага AppCore::exit); место вызова open в playback_manager — только отправка Open, индекс трека запоминается до события (ADR-01, ТЗ-103). Файлы: src/app/mod.rs, src/app/playback_manager.rs (только вызов open). Проверка: cargo check
[ ] Шаг 31: drain_engine_events в начале tick: очередь → AudioFacade::on_event (буфер событий переиспользуется); новый открытый трек → on_track_opened (ADR-02, И-Р13). Файл: src/app/mod.rs. Проверка: cargo check
[ ] Шаг 32: Разбор остальных Applied: Skipped → пропуск трека, OpenFailed → on_open_failed, Ended → переход к следующему (без двойного шага с опросом ended() в tick), DeviceLost/Notice → существующие обработчики (ADR-01, ТЗ-103). Файл: src/app/mod.rs. Проверка: cargo check
[ ] Шаг 33: Пробуждение UI по событию: install_drain_hook в main.rs — Weak<RefCell<MusicApp>>, upgrade + try_borrow_mut → drain_engine_events (занято — догонит tick) (ADR-02, ТЗ-60, ТЗ-106). Файл: src/main.rs. Проверка: cargo check
[ ] Шаг 34: Rc-цикл R-20: MusicApp хранит Weak<AppWindow> (upgrade в местах использования) (ADR-01, ТЗ-45). Файлы: src/app/mod.rs, src/main.rs. Проверка: cargo check
[ ] Шаг 35: Убрать вызов probe_output из app: окно показывается до проверки устройства, результат — событиями движка Devices / Notice::DevicesUnavailable (ТЗ-104). Файл: src/app/mod.rs. Проверка: cargo check
[ ] Шаг 36: Удалить busy-wait probe_output и PROBE_OPEN_MS (и их тесты) (ТЗ-104). Файл: src/audio/output.rs. Проверка: cargo check
[ ] Шаг 37: ЧЕКПОИНТ шагов 25–36 + ручной запуск cargo run за пользователем (играет в Совместимом режиме, переключение треков, ошибка файла видна). ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[ ] Шаг 38: EngineHandle::sender() → EngineSender (Clone + Send, send(cmd) -> bool) для владельцев вне MusicApp (ADR-01). Файл: src/engine/run.rs. Проверка: cargo check
[ ] Шаг 39: EngineSink в AppDeps (spec 02 §2.12) + RecordingEngine-фейк; main.rs передаёт EngineSender. Файлы: src/core/mod.rs, src/core/testing.rs, src/main.rs (одна строка AppDeps). Проверка: cargo check
[ ] Шаг 40: Ожидание ShutdownComplete: EngineEventQueue::wait_for(pred, timeout) (recv_timeout, прочие события не теряются — возвращаются в разбор) (§6.28 п. 3). Файл: src/app/engine_sink.rs. Проверка: cargo test engine_sink
[ ] Шаг 41: AppCore::exit: release_engine → deps.engine.send(Shutdown); запись settings/state не ждёт ShutdownComplete (ОВС-16, ТЗ-136, §6.28); комментарий «мост до С5/С6» обновить. Файл: src/core/mod.rs. Проверка: cargo check
[ ] Шаг 42: MusicApp::exit: ожидание ShutdownComplete до 2 с через wait_for, затем quit_event_loop; AudioFacade::shutdown на пути выхода убрать (ТЗ-45, §6.28). Файл: src/app/mod.rs. Проверка: cargo check
[ ] Шаг 43: Тесты выхода: существующие тесты на новый путь + shutdown_writes_settings_even_on_timeout (ТЗ-136). Файл: src/core/testing.rs. Проверка: cargo test core::
[ ] Шаг 44: ЧЕКПОИНТ шагов 38–43. ЧЕКПОИНТ: полный cargo test + cargo clippy (0 новых варнингов в файлах вайтлиста)
[ ] Шаг 45: Убрать мост AudioFacade в playback_manager: прямые engine.send/UiAudioState. Файл: src/app/playback_manager.rs. Проверка: cargo check
[ ] Шаг 46: Убрать мост AudioFacade в visualizer_manager и playlist_manager. Файлы: src/app/visualizer_manager.rs, src/app/playlist_manager.rs. Проверка: cargo check
[ ] Шаг 47: Убрать мост AudioFacade в bp_report. Файл: src/app/bp_report.rs. Проверка: cargo check
[ ] Шаг 48: Убрать мост AudioFacade в mod.rs (все обращения к self.player). Файл: src/app/mod.rs. Проверка: cargo check
[ ] Шаг 49: Удалить src/app/audio_facade.rs и его mod-строку; снять #![allow(dead_code)] в engine_sink.rs и ui_audio_state.rs. Файлы: src/app/audio_facade.rs, src/app/mod.rs, src/app/engine_sink.rs, src/app/ui_audio_state.rs. Проверка: cargo check без варнингов dead_code
[ ] Шаг 50: Финал: полный cargo test + clippy, tools/check_rt_imports.py, grep Player вне src/engine и src/audio, ROADMAP ✅, ручные проверки за пользователем (UI не блокируется, ошибки треков видны, Exclusive через старый путь, выход ≤ 2 с). Файлы: ROADMAP.md, _STATE_.yaml. Проверка: всё зелёное

- **Текущий шаг (current_step):** Шаг 31
- **Следующий ход:** Шаг 31: drain_engine_events в начале tick: engine_events.drain → player.on_event (AudioFacade); Applied, соответствующий открытию трека из pending_open → on_track_opened(index, prev_current, &info) (снять #[allow(dead_code)] с on_track_opened; после этого уйдёт временный варнинг dead_code AppEvent::TrackChanged) (mod.rs). Шаг 30 готов: player: AudioFacade, LegacyAudio из настроек, exit через shutdown, pending_open, окно ошибки при EngineFault (ТЗ-88). Между 30 и 31 воспроизведение временно не работает — события ещё не разбираются. Замечание к шагу 42: Drop EngineHandle шлёт Shutdown и join'ит поток на UI-потоке — выход должен сначала дождаться ShutdownComplete. Отметка шага: в _STATE_.yaml done: true у шага, current_step +1, затем python tools/state_tool.py render. Шаг 29 готов: движок запускается в main до AppCore, MusicApp хранит engine/engine_fault/engine_events. Замечание к шагу 42: Drop EngineHandle шлёт Shutdown и join'ит поток — на UI-потоке блокирует, выход должен сначала дождаться ShutdownComplete. Шаг 28 готов: on_track_opened/on_open_failed вынесены, bin-тесты 46 зелёные. Шаг 27 готов: AudioFacade (src/app/audio_facade.rs) — open асинхронный (Result<(), String>, TrackInfo приходит событием Opened), set_device игнорирует path/pos (движок знает сам), reservation_pending всегда false (нет промежуточного события), release_if_exclusive → новая мостовая EngineCmd::ReleaseExclusive (конец плейлиста/трей, V5.1-B5/B6; в С6 — release бэкенда).  Шаг 26 готов: SlintEventSink/EngineEventQueue/event_channel/slint_wake/install_drain_hook в src/app/engine_sink.rs, 5 тестов.  Шаг 25 готов: UiAudioState — begin_open(req_gen), apply(EngineEvent)->Applied{None,Skipped,OpenFailed,Ended,DeviceLost,Notice,ShutdownComplete}, take_{transport,position,track,stream,devices}_dirty (Written<T>: копия «записано»); устаревшие Opened/Skipped/OpenFailed (req_gen < latest) и Ended чужой сессии отбрасываются, Position — только для сессии latest_req_gen; TrackInfo/StreamDesc сравниваются по сессии, каталог — по generation; #![allow(dead_code)] до шага 28. Шаг 23 готов: src/engine/tests.rs — ui_handlers_do_not_block_on_slow_open, spawn_failure_reports_error (вариант apap-decode не покрыт: Player::open строит реальный cpal-поток до spawn декодера, ТЗ-114), device_catalog_enumerated_once, device_hotplug_single_enumeration (CountingShared), set_mode_settings_performs_no_io (MemStore без вызовов), shutdown_emits_complete (Shutdown и Drop). Шаг 22 готов: Engine::shutdown() — пауза если играет → release_engine (вывод: PCM, затем резервирование; затем декодер) → current=None, stopped → ShutdownComplete; PersistStore::save не вызывается — данных bp_tests/track_marks в движке моста нет (запись пустого затёрла бы файлы), добавится с владельцем (С8/С9). Шаг 21 готов: сеттеры Player для SetLegacyAudio только пишут поля (bit_perfect/dither переоткрывают поток — I/O устройства, не настроек), комментарий уточнён; тест MemStore без записей — шаг 23. Шаг 20 готов (062ce2d): EngineEvent::Devices(Arc<DeviceCatalog>) и Notice::DevicesUnavailable(BackendError); run() стартует DeviceWatcher (notify → AtomicBool devices_dirty) и перечисляет каталог с force_emit (стартовая проверка устройства в движке; удаление probe_output в app — шаг 30); RefreshDevices и devices_dirty в poll_session → Devices только при изменении; 3 теста движка. Шаг 19 готов: Open → deps.sources.probe (Err → Skipped File(e), Player не трогается) → player.open; Err + player.take_fault() → OpenFailed(Internal(fault)), иначе мост Skipped(Unsupported{msg}); Player хранит decode_events (Receiver после Ready) и last_fault, poll_decode_failure() в poll_session → Skipped (Io → ReadDuringPlayback{at_frame: 0 — мост}) + session_failed гасит Ended до следующего Open. Шаг 18 готов: poll_session() после каждой команды и на тайм-ауте 20 мс — poll_reservation → Notice(Reservation) + Failed → stop+OpenFailed(Capture(ReservationDenied{owner:None})), Lost → OpenFailed(Capture(ReservationLost)); ended() → Ended{session: req_gen} один раз + clear_end (движок — единственный потребитель флага конца); refresh_transport() с дедупом last_transport и флагом Engine.stopped (приоритет над has_decoder); Position при изменении и не чаще 100 мс (deps.clock, POSITION_INTERVAL). Шаг 17 готов: Engine{player, deps, current:(req_gen,path), catalog} в потоке apap-engine, recv_timeout 20 мс (Timeout → continue), EngineHandle::spawn/send/join/Drop(Shutdown+join); Open мост (Err → Skipped Unsupported), SetDevice Err → OpenFailed DeviceLost, RefreshDevices → catalog.refresh без события, SetLegacyAudio применяет все поля (шаг 21 — только проверка/тест), Shutdown → release_engine + ShutdownComplete. ВНИМАНИЕ для шага 18: legacy Player после stop() держит has_decoder()==true — состояние Stopped хранить в Engine, не выводить из has_decoder. Шаг 16 готов: EngineDeps{shared, watcher, sources, clock, store, events, spawner}, EngineDeps::system(events, dir: PathBuf, journal: Arc<dyn Journal>) — dir/journal строит main (ADR-19). ЧЕКПОИНТ 15 зелёный (cargo test 558 ok, clippy 0). Шаг 14 готов: platform::devwatch — PollWatcher<F>(snapshot, interval) поток apap-devwatch, событие только при изменении, Drop останавливает; NullDeviceWatcher; system_device_watcher() (Linux: /proc/asound/cards, 1 с). Шаг 13 готов: DeviceCatalog{shared, generation} (hw — позже с ExclusiveBackend), refresh(&mut dyn SharedBackend)->Result<bool> (generation только при изменении), find/default_device; trait DeviceWatcher; FakeDeviceWatcher + FakeWatchTrigger{fire, started}. Шаг 12 готов: SharedBackend::enumerate (подмножество С3), SharedDeviceId/SharedDeviceInfo{legacy: DeviceInfo — мост}/BackendError в backend/mod.rs, CpalSharedBackend<H: AudioHost>, FakeSharedBackend, fake_device. ЧЕКПОИНТ 11 зелёный (cargo test 548 ok, clippy 0). Шаг 10 готов: SymphoniaSourceOpener (probe: dsf/dff → probe_dsd_format, иначе probe_header; open: DsdDecoder/Decoder, String → FileError мост). Шаг 9 готов: dsd::probe_dsd_format(path) -> Result<SourceFormat, FileError>. Шаг 8 готов: decoder::probe_header(path) -> Result<SourceFormat, FileError> (прежний шаг 8 разбит на 8–10, нумерация сдвинута на +2). Шаг 7 готов: SourceOpener, FakeSource (with_format/with_error/with_probe_delay, probes/opens). Шаг 6 готов: DecodeWorker::spawn(&dyn ThreadSpawner) -> Result<_, EngineFault>, Player.spawner. Решения пользователя 2026-10-07: мост SetLegacyAudio (вместо SetModeSettings/SetActiveMode до С4); ModeKind временно в src/settings.rs. Player — внутренняя деталь apap-engine (мост). Тесты — только на ЧЕКПОИНТ-шагах.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

- [~] **7.** Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче
  - Инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
- [>] **13.** Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА
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
