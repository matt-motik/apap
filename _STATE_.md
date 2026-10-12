<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->

# Состояние сессии

- **Текущая задача:** Нет (все шаги завершены)
- **Состояние:** done

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

- [~] **7.** Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче
  - Инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
- [>] **13.** Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА
  - Закрытые этапы и баги — в ROADMAP.md (✅ с хэшем); здесь только открытое.
  - AM1.0-8.4 (С4 (01) «Модель режимов и настроек») закрыт в c078c9c (2026-10-13, решение пользователя: закрыть с хвостом Linux-проверок, см. строку «Ручные проверки … AM1.0-8.4»); СЛЕДУЮЩИЙ (п. 15 сквозного порядка): SP1.0-8.9 (С9 (02) «Диалог настроек»).
  - Ручные проверки за пользователем: AM1.0-8.4 — (Linux) полный cargo test зелёный (на Windows на шаге 56: build и clippy --all-targets чистые, test — только 6 падений SP1.0-B7); НУЖНА ПРОВЕРКА: Строгий режим на ADI через hw: (PCM и DSD-трек) — не проверялся; DSD-трек на ADI в Оптимальном через hw: — не проверялся; беглый повтор режимов после шагов 51–57 (только удаления и снятие serde). Подтверждено пользователем 2026-10-13: на Linux на коде до шага 51 переключатель режимов, Совместимый, Оптимальный/Строгий через hw: работали.
  - ТЗ-97 (решение пользователя 2026-10-13): settings.toml удалять можно. На Windows-ноутбуке файл убран (переименован в settings.toml.bak) 2026-10-13; на Linux-десктопе — удалить ~/.config/music_player/settings.toml перед запуском (за пользователем).
  - ВОПРОС ПОЛЬЗОВАТЕЛЮ (AM1.0-8.4 шаг 2): HwDeviceId (§2.3) ещё нет в коде; в OptimalOpts/StrictOpts.device временно Option<String>. Предложение: добавить newtype HwDeviceId в src/audio/backend/mod.rs (расширить вайтлист) и заменить. Умолчание BufferMs = 1500 мс (как RING_BUFFER_MS_DEFAULT; спека числа не задаёт).
  - AM1.0-8.4 шаг 29: таблиц подсказок SRC_FILTER_INFO/DSD_FILTER_INFO (§6.27) в коде нет — подсказки вариантов фильтров в mode_dialog пустые; строки Device/Lock/Test и др. без каталога — RowKind::Info. Дозаполнить при подключении диалога или отдельным шагом.
  - ВОПРОС ПОЛЬЗОВАТЕЛЮ (AM1.0-8.4 шаг 10): legacy_path отображает SrcFilter на старый ResamplerAlgorithm по крутизне: Steep→SincSlow(128), Slow→SincMedium(64), VerySlow→SincFast(32) — решение шага, в спеке нет; Оптимальный → ExclusiveMode::Strict (без перехода в Shared); DsdFilter/dsd_gain_comp/volume_lock/device_buffer старый путь не параметризуют.
  - AM1.0-8.4 шаг 13: SetActiveMode в движке только сохраняет параметры (release_engine + поля), переоткрытие — последующий Open от UI (§6.18 стр. 2399, ТС-9). Пробел: при возврате на SystemDefault (legacy_audio → device None) Player.preferred_device не сбрасывается — нет API в player.rs (вне вайтлиста); закрыть на С5 или расширить вайтлист.
  - AM1.0-8.4 шаг 14: тест audio_param_change_reopens_at_position_preserving_pause (§7.2 стр. 1834) не написан — Engine владеет настоящим Player (Decoder + cpal), фейка нет (ТЗ-114); проверено только решение mode_apply_action. Дописать, когда путь станет фейкуемым через EngineDeps (С5+).
  - ВОПРОС ПОЛЬЗОВАТЕЛЮ (AM1.0-8.4 шаг 17): OpenError::ModeUnavailable в classify() → ErrorClass::Internal (в спеке отдельного класса нет) → реакция StopWithError во всех режимах (стоп, без перехода к следующему треку). Подтвердить или задать класс.
  - AM1.0-8.4 после шага 18 (временно, до шагов 21–30): старый диалог аудио-настроек меняет только локальный кэш фасада и в движок не попадает. Не считать багом при ручном запуске.
  - AM1.0-8.4 шаг 7: имена ключей [playback] dither (tpdf/off), rate_fallback (same_family/nearest/no_downsample), volume_lock, fixed_rate (Гц), buffer (мс), device (строка) спекой поимённо не заданы — взяты по именам полей ModeSettings; подтвердить пользователю.
  - На С5 (решение пользователя 2026-10-08): снять мост src/app/audio_facade.rs — вызовы в playback_manager, visualizer_manager, playlist_manager, bp_report, mod.rs → прямая работа с EngineHandle/SignalPath/BadgeState; удалить файл; снять #![allow(dead_code)] в engine_sink.rs и ui_audio_state.rs. Состояние фасада (req_gen, кэш транспорта/трека/потока, ended, очередь резервирования, volume/muted/LegacyAudio) переносить целиком, не по файлам.
  - Ручные проверки за пользователем: AM1.0-8.1 — сценарии ТЗ-1/2/48/118/119/120/122 — отложено до полной реализации замка (решение пользователя 2026-10-10).
  - На С5/С6 (решение пользователя 2026-10-10): BackendCaps брать из аудио-бэкенда (§2.3, src/audio/backend/mod.rs), перенести туда тип из src/engine/run.rs; убрать pub(crate) EngineHandle::spawn_with_caps (временная точка подстановки caps для тестов шага 17 AM1.0-8.4, т.к. src/engine/deps.rs не был в вайтлисте).
  - Windows-ноутбук: полный cargo test красный на 6 lib-тестах с POSIX-путями (SP1.0-B7), воспроизводится без правок этапа; критерий шагов на Windows — нет новых падений сверх этих шести. Зелёный полный прогон AM1.0-8.4 подтверждать на Linux.
  - Отклонения старого пути до С6: повтор EBUSY на тике 100 мс; select_output_for может кратко пробовать hw: до резервирования.
  - На С6: баг AM1.0-B1 (устройство не возвращается в PipeWire после hw:, EBUSY при пересоздании узла); тест unreadable_playlist_never_written (в AppCore нет чтения плейлиста) — по §8 стартовое чтение в AppCore приходит в С8 (02), не в С6 (02).
  - На С9 (с переносом диалога и шлюза в AppCore): тесты picker_does_not_block_loop, cache_size_counted_once_per_dialog, cache_clear_survives_cancel (на С7 нет dialog_open/gate в AppCore); perf_picker_open_30s — на С12.
  - VizCycle (viz_settings_manager.rs) защищён только Slint-оверлеем.
  - Ручные проверки за пользователем: SP1.0-8.5 — SIGTERM/SIGINT/SIGHUP (kill) сохраняют state/settings, второй сигнал завершает сразу (выход из трея и уведомление при ошибке записи подтверждены 2026-10-10).
  - На Windows-ноутбуке/macOS: собрать и проверить SP1.0-8.5 (WM_ENDSESSION-сабкласс windows.rs, applicationShouldTerminate: macos.rs, отсутствие трея) — локально не компилировалось.
  - Строка tray_works_during_dialog: проверка SetModeSettings и колёсика — на С11/с модулем режимов.
  - SP1.0-B5 вынес из С9 перенос кнопок плейлиста в меню; на С9 остаётся остальное. Текст ТЗ-34 «Удалить текущий трек» расходится с реализацией (выделенный) — правка docs по разрешению пользователя.
  - На Windows-ноутбуке: проверить cargo build (Windows/macOS проверены только ревью).
  - На С12: ручной strace ТЗ-19.

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
