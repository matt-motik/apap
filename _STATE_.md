<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** SP1.0-8.10 — С10 (02). Громкость и mute в UI (ТЗ-35, 37, 38, 39; НФ-2, НФ-3)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/core/volume.rs
  - src/core/mod.rs
  - src/core/testing.rs
  - src/persist/state_file.rs
  - src/app/mod.rs
  - src/app/playback_manager.rs
  - ui/top_panel.slint
  - ui/app.slint
- **Критерий успеха (Definition of Done):** Все источники громкости идут через VolumeCmd и одну функцию применения (ADR-14); блокировка в Строгом и при volume_lock не меняет громкость/состояние/срок N; mute пишется в PlaybackState по режимам; подсказки §2.13 через Tooltip; тесты §7.2 этапа зелёные; cargo build/clippy --all-targets чистые; cargo test — без новых падений сверх 6 тестов SP1.0-B7 (Windows).

## Итерационный трекер
- [x] Шаг 1: src/core/volume.rs (новый) + pub mod в src/core/mod.rs: VolumeCmd, VolumeSource, VolumeBlock, VolumeOutcome, Clicks (временно здесь до platform/input, С11), VOLUME_STEP_PCT, volume_block, apply_volume, VolumeBlock::tooltip (§2.11, §2.13, ADR-14, НФ-2, ТЗ-37, ТЗ-39). Проверка: cargo check без ошибок
- [x] Шаг 2: Юнит-тесты чистых функций в src/core/volume.rs: volume_block_tooltips, Set/Step с clamp 0…100, блокировка в Строгом, блокировка при volume_lock, Unchanged при том же значении (§7.2, И-Р13). Проверка: cargo test core::volume зелёный
- [x] Шаг 3: src/persist/state_file.rs: геттер SessionState::playback_state() -> &PlaybackState (вайтлист расширен по разрешению пользователя 2026-10-13); src/core/mod.rs: AppCore::volume(src, cmd) -> VolumeOutcome (volume_block до любого изменения, И-Р13; Changed → change_state(User, Volume)) и AppCore::toggle_mute() -> bool (change_state(User, Muted), работает при блокировке) (§2.12, ADR-14, ТЗ-37, ТЗ-38). Проверка: cargo check без ошибок
- [x] Шаг 4: Тесты ядра в src/core/testing.rs, ч. 1: slider_shows_active_mode_volume, strict_mode_wheel_and_hotkeys_do_not_change_volume, optimal_volume_lock_ignores_all_sources (§7.2, ТЗ-35, ТЗ-37). Проверка: cargo test по этим трём именам зелёный
- [x] Шаг 5: Тесты ядра в src/core/testing.rs, ч. 2: mute_per_mode_survives_restart; mute_starts_timer дополнить путём через AppCore::toggle_mute (§7.2, ТЗ-38). Проверка: cargo test mute_ зелёный
- [x] Шаг 6: Ползунок → VolumeCmd::Set: MusicApp::apply_volume_cmd(src, cmd) в src/app/playback_manager.rs (core.volume → при Changed player.set_volume + AppEvent::VolumeChanged; свойство ползунка выставляется безусловно после каждой команды), on_volume_changed в src/app/mod.rs вызывает его (ADR-14, ТЗ-35). Проверка: cargo check без ошибок
- [x] Шаг 7: Slint: объявить и пробросить callback volume-step(int) в ui/top_panel.slint и ui/app.slint; привязка ползунка value <=> volume (значение приходит только из Rust, ТЗ-35, ADR-14). Проверка: cargo check без ошибок
- [>] **Шаг 8: Колёсико над ползунком → VolumeCmd::Step(SliderWheel): on_volume_step в src/app/mod.rs; в ui/top_panel.slint колёсико вызывает volume-step(±1), расчёт ±0,04 удалён (НФ-2, ADR-14, §8.1 «удаляется»). Проверка: cargo check без ошибок**
- [ ] Шаг 9: Трей → VolumeCmd::Step(TrayWheel) в src/app/mod.rs: TrayEvent::Scroll идёт через apply_volume_cmd, WHEEL_VOLUME_STEP и собственные проверки блокировки удалены; знак щелчка сырой дельты сохраняется до нормализации С11 (ADR-14, ТЗ-24, НФ-2). Проверка: cargo check без ошибок
- [ ] Шаг 10: Mute в состояние сессии: on_toggle_mute в src/app/mod.rs → core.toggle_mute() + player.set_muted; кнопка mute в ui/top_panel.slint доступна в Строгом режиме и при фиксации (ТЗ-37, ТЗ-38, ADR-14). Проверка: cargo check без ошибок
- [ ] Шаг 11: Slint: свойство volume-block-text (string) в ui/top_panel.slint и ui/app.slint; Tooltip с этим текстом на неотключаемой обёртке ползунка (ТЗ-39, ADR-14, §2.13). Проверка: cargo check без ошибок
- [ ] Шаг 12: Текст подсказки из ядра: src/app/mod.rs выставляет volume-block-text из volume_block(...).tooltip() вместе с volume-enabled; самодельная подсказка «Громкость задана режимом» в ui/top_panel.slint удалена (ТЗ-39, §2.13). Проверка: cargo check без ошибок
- [ ] Шаг 13: ЧЕКПОИНТ: cargo build, cargo clippy --all-targets (0 новых предупреждений в файлах вайтлиста), полный cargo test (без новых падений сверх 6 тестов SP1.0-B7); grep: нет WHEEL_VOLUME_STEP и 0.04 в путях громкости. Проверка: все три команды с ожидаемым итогом

Легенда: [x] сделано · [>] текущий шаг · [ ] не начато

- **Текущий шаг (current_step):** Шаг 8
- **Следующий ход:** Шаг 8: субагент добавляет on_volume_step в src/app/mod.rs и переводит колёсико ползунка на volume-step в top_panel.slint
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

- [~] **7.** Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче
  - Инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
- [>] **13.** Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА
  - Закрытые этапы и баги — в ROADMAP.md (✅ с хэшем); здесь только открытое.
  - SP1.0-8.9 (С9 (02) «Диалог настроек») закрыт в 9d00f27 (2026-10-13, Windows-ноутбук); СЛЕДУЮЩИЙ (п. 16 сквозного порядка): SP1.0-8.10 (С10 (02) «Громкость и mute в UI»). AM1.0-8.4 закрыт в c078c9c с хвостом Linux-проверок (строка «Ручные проверки … AM1.0-8.4»).
  - Ручные проверки за пользователем: SP1.0-8.9 — (GUI на этапе не запускался) ТЗ-27: «Сохранить» неактивна без отличий и после «изменить и вернуть», подсказка при наведении на неактивную кнопку (не обрезается, ряд кнопок не сдвинут), поведение при выборе темы (загрузка/битая тема/плохой HEX); ТЗ-34: текст на вкладке кэша, очистка не отменяется «Отменой»; ТЗ-23: при открытом диалоге и окне сообщения команды главного окна не срабатывают (в т. ч. клавиша V, меню «Визуализация», переключатель режимов); ТЗ-25/28/29: «Сохранить» применяет только изменённое, «Отмена» ничего не меняет; (Linux) полный cargo test зелёный (на Windows: build и clippy --all-targets чистые, test — только 6 падений SP1.0-B7).
  - Находки SP1.0-8.9 — SP1.0-B8…B15 в ROADMAP.md. ВОПРОС ПОЛЬЗОВАТЕЛЮ: SP1.0-B8 (визуализация читает черновик до «Сохранить») — править код или узаконить предпросмотр в ТЗ-25/26; в §6.9 спецификации нет веток применения для TopPanel/InfoLabels/ScrollToPlaying (в коде есть) — правка docs по разрешению.
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
  - На С12: perf_picker_open_30s.
  - Ручные проверки за пользователем: SP1.0-8.5 — SIGTERM/SIGINT/SIGHUP (kill) сохраняют state/settings, второй сигнал завершает сразу (выход из трея и уведомление при ошибке записи подтверждены 2026-10-10).
  - На Windows-ноутбуке/macOS: собрать и проверить SP1.0-8.5 (WM_ENDSESSION-сабкласс windows.rs, applicationShouldTerminate: macos.rs, отсутствие трея) — локально не компилировалось.
  - Строка tray_works_during_dialog: проверка SetModeSettings и колёсика — на С11/с модулем режимов.
  - Текст ТЗ-34 «Удалить текущий трек» расходится с реализацией (удаляется выделенный, SP1.0-B6) — правка docs по разрешению пользователя.
  - На Windows-ноутбуке: проверить cargo build (Windows/macOS проверены только ревью).
  - На С12: ручной strace ТЗ-19.

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
