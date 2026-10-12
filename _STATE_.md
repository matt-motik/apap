<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** SP1.0-8.9 — С9 (02). Диалог настроек: DialogDraft, diff, validate, apply_plan, «Сохранить»/«Отмена» по §6.9, шлюз (ТЗ-23 (полностью), 25, 26, 27, 28, 29, 30, 31, 34)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/core/dialog.rs
  - src/core/mod.rs
  - src/core/testing.rs
  - src/main.rs
  - src/app/mod.rs
  - src/app/ui_manager.rs
  - src/app/playlist_manager.rs
  - src/app/viz_settings_manager.rs
  - src/app/fulltrack_manager.rs
  - src/app/audio_facade.rs
  - ui/settings.slint
  - ui/app.slint
  - ROADMAP.md
  - _STATE_.yaml
  - _STATE_.md
- **Критерий успеха (Definition of Done):** cargo build/test/clippy зелёные (на Windows — нет новых падений сверх 6 SP1.0-B7; 0 новых варнингов в файлах вайтлиста); тесты §7.2 этапа С9 зелёные; «Сохранить» — одна запись settings.toml и применение только отличающегося; «Отмена» ничего не меняет; все команды главного окна и V идут через шлюз; settings_draft/белого списка полей нет

## Итерационный трекер
- [x] Шаг 1: src/core/dialog.rs (новый; `pub mod dialog;` в src/core/mod.rs): типы DialogDraft {settings, viz_mode, reset_widths, shown_mode, theme}, ThemeLoad, SettingsField, SettingsDiff (+ is_empty), Invalid — строго по §2.10 (ADR-11, ТЗ-25…29). Без функций. Проверка: cargo check
- [x] Шаг 2: src/core/dialog.rs: diff(cur, state, draft) -> SettingsDiff (§2.10, §6.9, ТЗ-27, ТЗ-29, ТЗ-30): поля SettingsField сравнением с действующими; modes — через ModeSettings::diff (None при равенстве); viz_mode; reset_widths — отличие только если нажата и в состоянии есть заданные ширины; shown_mode не сравнивается; тема — ThemeLoad, отличный от Unchanged. Тесты (≤5): пустой diff нетронутого черновика, только тема, reset_widths без ширин в состоянии, shown_mode не даёт отличия, параметр режима. Проверка: cargo test core::dialog
- [x] Шаг 3: src/core/dialog.rs: validate(draft) -> Vec<Invalid> (§2.10, ТЗ-27): ThemeLoad::Loading и ThemeLoad::Invalid — недопустимы; сочетание буферов режима — через settings::playback::buffers_allowed (param — имя параметра). Тесты (≤4): корректный черновик пуст, Loading, Invalid, недопустимые буферы содержат имя параметра. Проверка: cargo test core::dialog
- [x] Шаг 4: src/core/dialog.rs: ApplyStep и apply_plan(diff, draft) -> Vec<ApplyStep> (§2.10, §6.9, ТЗ-29, §5.2 ТЗ): шаг только для отличающегося; Theme(Arc<ThemeData>) из ThemeLoad::Ready; Columns{visibility, order, defs}; Engine(ModeSettingsUpdate) только при diff.modes = Some. Тесты (≤5): пустой diff → пустой план, только тема, только колонки, только размер обложки (TopPanel, без Engine), параметр режима → один Engine. Проверка: cargo test core::dialog
- [x] Шаг 5: AppDeps.io: Box<dyn IoWorker> + AppCore::io() (§2.12, ADR-20) — src/core/mod.rs; Harness передаёт FakeIo (src/core/testing.rs); src/main.rs — одна строка создания (временный мост: второй экземпляр apap-io до шага 6). ОБЩИЙ ТИП — Проверка: cargo test --no-fail-fast (нет новых падений сверх 6 SP1.0-B7)
- [x] Шаг 6: src/app/mod.rs: MusicApp работает с apap-io через core.io() (submit/try_recv), собственное поле io и его создание удаляются — мост шага 5 снят (ADR-20, §2.12). Проверка: cargo check; grep spawn_io — одно создание
- [x] Шаг 7: src/core/mod.rs: поле gate: UiGate в AppCore + gate()/gate_mut() (§2.12, ADR-12, ТЗ-23). Проверка: cargo check
- [x] Шаг 8: src/app/mod.rs, src/app/playlist_manager.rs: шлюз MusicApp → core.gate()/gate_mut(), поле MusicApp.gate удаляется (ADR-12, И-Р8). Проверка: cargo check; grep `gate: UiGate` только в src/core
- [x] Шаг 9: src/core/mod.rs: draft: Option<DialogDraft>; dialog_open (черновик из settings/state, gate.block(Dialog), io.submit(CacheSizes); при блокировке — без действий), dialog_edit(f), draft(), dialog_cancel (draft = None, unblock), clear_cache(which) → io.submit(ClearCache) вне черновика (§6.9, ТЗ-23, ТЗ-26, ТЗ-34, ADR-11). Проверка: cargo check
- [x] Шаг 10: Тесты диалога, ч. 1 (src/core/testing.rs, §7.2): menu_inactive_while_dialog_open (каждая MainCmd + повторное открытие — 0 изменений), event_dialog_edit_writes_nothing, cache_size_counted_once_per_dialog, dialog_cancel_leaves_settings_and_file_unchanged, cache_clear_survives_cancel (ТЗ-22, 23, 26, 34). Проверка: cargo test core::testing
- [x] Шаг 11: src/core/mod.rs: dialog_save (§6.9, ТЗ-25, 28, 29, 30, 32): diff/validate → пусто или недопустимо — без действий; итоговые настройки одной записью (save_settings_now); tracker.set_interval; change_state(User, VizMode); change_state(User, ColumnWidths(∅)) при reset_widths; engine.send(SetModeSettings(u)) только при modes = Some; draft = None, unblock; возврат — план ApplyStep + эффект записи. save_button() -> {enabled, tooltip} (ТЗ-27). exit() сбрасывает черновик (ТЗ-32, §6.10). Проверка: cargo check
- [x] Шаг 12: Тесты диалога, ч. 2 (src/core/testing.rs, §7.2): save_button_disabled_without_changes, save_button_invalid_value_tooltip, dialog_mode_selector_does_not_switch_mode, save_keeps_state_changed_during_dialog, reset_widths_goes_to_state (ТЗ-25, 27, 30). Проверка: cargo test core::testing
- [x] Шаг 13: Тесты диалога, ч. 3 (src/core/testing.rs, §7.2): dialog_save_applies_only_edited_fields, save_write_error_closes_dialog_and_applies, exit_with_open_dialog_discards_draft (на каждую причину выхода); существующие dialog_save_single_write_all_fields и dialog_save_without_changes_does_not_write переводятся на dialog_open/dialog_edit/dialog_save (ТЗ-20, 28, 29, 32). Проверка: cargo test core::testing
- [x] Шаг 14: Тесты шлюза, ч. 4 (src/core/testing.rs, §7.2): menu_inactive_while_message_open и menu_inactive_while_picker_open на шлюзе AppCore, picker_does_not_block_loop, clear_playlist_menu_marks_dirty (ТЗ-23, 34, 52, 53). Проверка: cargo test core::testing
- [x] Шаг 15: src/app/viz_settings_manager.rs, src/app/fulltrack_manager.rs: прямые обращения к полю MusicApp.dialog → через хелперы cfg()/edit_cfg()/dialog_mut() (подготовка к снятию локального черновика; ADR-11). Проверка: cargo check; grep обращений к полю dialog в этих файлах пуст
- [>] **Шаг 16: src/app/mod.rs, src/app/ui_manager.rs: локальный struct DialogDraft и поле dialog удаляются; хелперы cfg()/cfg_viz_mode()/dialog_mut()/edit_cfg()/open_dialog_draft() — поверх core.draft()/dialog_edit()/dialog_open(); column_widths → reset_widths, mode_edit → shown_mode (§2.10, §8.1 С9 «удаляется settings_draft»). Сохранение пока прежним кодом (мост до шага 18). Проверка: cargo check**
- [ ] Шаг 17: src/app/mod.rs: выбор темы в диалоге — DialogDraft.theme: ThemeLoad (Loading{gen} при отправке ReadTheme, Ready/Invalid по ответу apap-io с тем же gen); поля theme_selection и theme_reading удаляются (§2.10, §6.9, ТЗ-27, ТЗ-29). Проверка: cargo check; grep theme_selection и theme_reading пуст
- [ ] Шаг 18: src/app/mod.rs, src/app/audio_facade.rs: on_settings_save → core.dialog_save() и исполнение шагов ApplyStep (тема без чтения файла, колонки без смены ключа/порядка, обложки, визуализация, бюджет кэша, трей; Engine(u) — фасад только принимает новые ModeSettings в кэш, команда уже отправлена ядром); on_settings_close/Esc → core.dialog_cancel(); прежняя замена настроек целиком удаляется (§6.9, §5.2 ТЗ, ТЗ-26, 28, 29, 31). Проверка: cargo test --no-fail-fast (нет новых падений)
- [ ] Шаг 19: ui/settings.slint, ui/app.slint: свойства save-enabled и save-tooltip у кнопки «Сохранить» (неактивна без отличий и при недопустимом значении; подсказка с именем параметра) — рядом с действующим theme-save-enabled (ТЗ-27). Проверка: cargo check
- [ ] Шаг 20: src/app/mod.rs: синхронизация save-enabled/save-tooltip из core.save_button() после открытия диалога, каждой правки черновика и ответа apap-io по теме; set_theme_save_enabled больше не вызывается (ТЗ-27, §6.9). Проверка: cargo check
- [ ] Шаг 21: ui/settings.slint, ui/app.slint: удалить свойство theme-save-enabled (заменено save-enabled; ТЗ-27). Проверка: cargo check; grep theme-save-enabled и theme_save_enabled пуст
- [ ] Шаг 22: ui/settings.slint: текст на вкладке кэша «Очистка выполняется сразу и не отменяется кнопкой „Отмена“» (§2.13, ТЗ-34). Проверка: cargo check
- [ ] Шаг 23: src/app/viz_settings_manager.rs: клавиша V (cycle_viz) и пункты меню «Визуализация» (menu_select_viz) — через core.gate().allows(MainCmd::VizCycle) (ТЗ-23, ADR-12, И-Р8). Проверка: cargo check
- [ ] Шаг 24: ЧЕКПОИНТ: cargo build; cargo test --no-fail-fast (только 6 падений SP1.0-B7); cargo clippy --all-targets — 0 новых предупреждений в файлах вайтлиста; каждый обработчик команды главного окна спрашивает шлюз (сверка с перечнем MainCmd); ручные проверки ТЗ-23/27/34 — пользователю. Проверка: всё перечисленное зелёное

Легенда: [x] сделано · [>] текущий шаг · [ ] не начато

- **Текущий шаг (current_step):** Шаг 16
- **Следующий ход:** Шаг 16: app/mod.rs, ui_manager.rs — локальный DialogDraft и поле dialog удаляются, хелперы поверх core (column_widths -> reset_widths, mode_edit -> shown_mode; сохранение прежним кодом — мост до шага 18); cargo check --all-targets
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

- [~] **7.** Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче
  - Инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
- [>] **13.** Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА
  - Закрытые этапы и баги — в ROADMAP.md (✅ с хэшем); здесь только открытое.
  - AM1.0-8.4 (С4 (01) «Модель режимов и настроек») закрыт в c078c9c (2026-10-13, решение пользователя: закрыть с хвостом Linux-проверок, см. строку «Ручные проверки … AM1.0-8.4»); СЛЕДУЮЩИЙ (п. 15 сквозного порядка): SP1.0-8.9 (С9 (02) «Диалог настроек») — начат 2026-10-13 на Windows-ноутбуке.
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
