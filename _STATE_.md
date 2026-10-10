<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** AM1.0-8.4 — С4 (01). Модель режимов и настроек: ModeSettings, PARAMS, load_mode_settings без миграции (ТЗ-18, 19, 93–97, 124, 109–111, 128–131, 135, 137, 140)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/settings.rs
  - src/settings/playback.rs
  - src/settings/params.rs
  - src/settings/playback_dto.rs
  - src/engine/legacy_path.rs
  - src/engine/mod.rs
  - src/engine/messages.rs
  - src/engine/run.rs
  - src/engine/tests.rs
  - src/persist/settings_file.rs
  - src/persist/state_file.rs
  - src/core/mod.rs
  - src/core/testing.rs
  - src/main.rs
  - src/app/audio_facade.rs
  - src/app/mod.rs
  - src/app/ui_manager.rs
  - src/app/playback_manager.rs
  - src/app/bp_report.rs
  - src/app/mode_dialog.rs
  - src/audio/output.rs
  - src/audio/fulltrack.rs
  - src/platform/tray/mod.rs
  - ui/top_panel.slint
  - ui/app.slint
  - ui/settings.slint
  - ui/mode_params.slint
  - ROADMAP.md
  - _STATE_.yaml
  - _STATE_.md
- **Критерий успеха (Definition of Done):** cargo build/test/clippy зелёные (0 новых варнингов в файлах вайтлиста); тесты §7.2 этапа С4 зелёные; три режима с новыми настройками (Совместимый — все ОС, Оптимальный/Строгий — Linux через старый exclusive-путь с резервированием); удалены ExclusiveMode/FallbackPolicy/ResamplerMode/FallbackRatePolicy/ClockFamily/bit_perfect/DsdMode/TargetBitDepth/TargetSampleRate/Triangular как пользовательские настройки, resampler_fixed_fallback_guard, validate_audio_settings; пользователю напомнено удалить settings.toml (ТЗ-97)

## Итерационный трекер
- [x] Шаг 1: src/settings/playback.rs (новый; `pub mod playback;` в src/settings.rs, ModeKind переезжает сюда, в settings.rs — `pub use playback::ModeKind`): перечисления ModeKind, SharedDeviceChoice (SharedDeviceId из audio::backend), RateFallbackRule, SrcFilter, DsdFilter, DeviceBuffer (ms(), period_ms() = ms/4), Dither, DsdAboveDac; newtype Volume(u8) 0..=100 и BufferMs(u16) 100..=10000 с проверяющими конструкторами (03_spec 01 §2.2 стр. 670–900; ТЗ-128…131, ОВС-6, ОВС-7, ОВС-13). Тест device_buffer_values_and_default (§7.2). Проверка: cargo test settings::playback
- [x] Шаг 2: src/settings/playback.rs: CompatibleOpts, OptimalOpts (volume_lock=false, dsd_gain_comp=true), StrictOpts, ModeSettings + Default (умолчания — матрица 02_tz.md §5 и §2.2), PlaybackState/ModeGain (Default: Compatible, громкости 100 %, ОВ-5, ТЗ-124), PathPolicy::active (§2.2, ОВС-15, ОВС-19, ADR-04). Тесты dsd_filter_and_comp_defaults и умолчание SrcFilter::Steep (первая часть src_filter_default_and_min_phase_not_selectable). Проверка: cargo test settings::playback
- [x] Шаг 3: src/settings/playback.rs: buffers_allowed(BufferMs, DeviceBuffer) -> Result<(), ValueBlocker> и ValueBlocker {PlayerBufferTooSmall{need_ms}, DeviceBufferTooLarge{max_ms}} — правило BufferMs ≥ 2 × DeviceBuffer (И-Р22, ТЗ-94, ТЗ-131, §6.27). Тест device_buffer_vs_ring_constraint (§7.2, стр. 1830). Проверка: cargo test settings::playback; ЧЕКПОИНТ: cargo test + clippy
- [x] Шаг 4: src/settings/params.rs (новый, `pub mod params` в src/settings.rs): ParamId (39 вариантов §2.2), Availability, FixedValue/UnavailableWhy/SettingsAnchor (в спеке не раскрыты — минимально: показываемое значение, ключ причины, вкладка+элемент), ApplyKind, ParamDescriptor, static PARAMS по матрице 02_tz.md §5 (стр. ~1202) и столбцу применения §6.18 (стр. 2383–2415), availability(id, mode) (ТЗ-95, ОВ-28, ОВС-17). Проверка: cargo check
- [x] Шаг 5: Тест params_table_matches_matrix (§7.2 стр. 2895) в src/settings/params.rs: эталон — матрица 02_tz.md §5, все строки × 3 режима; каждый ParamId ровно один раз в PARAMS. Проверка: cargo test settings::params
- [x] Шаг 6: src/settings/params.rs: ModeSettingsDiff, ModeSettingsUpdate (§2.2, ОВС-14); ModeSettings::diff(&self, new) -> ModeSettingsDiff (пары (режим, ParamId) изменённых полей); ModeSettingsDiff::strongest(active) -> ApplyKind — только параметры активного режима, SwitchDevice > ReopenAtPosition > Memory (И-Р26, §6.18). Тесты ≤4, включая every_audio_setting_changes_observable в части модели (§7.2 стр. 2896: каждое поле → свой ParamId). Проверка: cargo test settings::params; ЧЕКПОИНТ: cargo test + clippy
- [x] Шаг 7: src/settings/playback_dto.rs (новый, `pub mod playback_dto`): PlaybackDto {schema, compatible, optimal, strict, #[serde(flatten)] unknown}, CompatibleDto/OptimalDto/StrictDto с полями Option<toml::Value> + flatten unknown (ошибка значения — заметка по ключу, 02 spec §2.3 вариант В); ключи и строковые формы: §6.27 (src_filter "steep"/"slow"/"very_slow", dsd_filter 24/30/50, dsd_gain_comp, device_buffer_ms 20…400) и 02 spec §2.4; playback_dto(&ModeSettings) -> PlaybackDto со schema = Some(2) (02 spec §2.6 стр. 1274). Проверка: cargo check
- [x] Шаг 8: src/settings/playback_dto.rs: load_mode_settings(Option<PlaybackDto>) -> (ModeSettings, Vec<LoadNote>) пп. 1–3 §6.27 (стр. 2618–2642; LoadNote из persist::keys): None → default без заметок; schema ≠ 2 → default + одна заметка; по ключам: недопустимое → default + заметка, "remodulate" → ConvertToPcm + заметка, steep_short_delay/slow_short_delay → Steep + заметка, неизвестный ключ (в т.ч. active, src_quality) → игнор + заметка (ТЗ-97, ТЗ-94, ТЗ-34, ТЗ-130, ТЗ-124). Тесты missing_file_starts_compatible, legacy_settings_load_defaults_one_note, unknown_key_ignored_logged, remodulation_loads_as_convert, src_filter_default_and_min_phase_not_selectable (часть min-phase). Проверка: cargo test settings::playback_dto
- [x] Шаг 9: src/settings/playback_dto.rs: load_mode_settings п. 4 — согласование буферов Оптимального и Строгого: buffers_allowed ошибка → device_buffer := Ms40, иначе наименьшее допустимое; заметка «буфер устройства N мс больше половины буфера плеера M мс — установлено K мс» (И-Р22, ТЗ-131). Тесты: коррекция; круговой load_mode_settings(Some(playback_dto(m))) == (m, []) для умолчаний и не-умолчаний. Проверка: cargo test settings::; ЧЕКПОИНТ: cargo test + clippy
- [x] Шаг 10: src/engine/legacy_path.rs (новый, `mod legacy_path` в src/engine/mod.rs): адаптер legacy_audio(&ModeSettings, ModeKind) -> (LegacyAudio, device) (ОВС-12, §8 С4): Совместимый → exclusive Off, fixed_rate → ResamplerMode::Fixed, dither, buffer → ring_buffer_ms, DSD → PCM; Оптимальный → старый exclusive-путь без перехода в Shared (отказ → ошибка, решение 4), RateFallbackRule → FallbackRatePolicy; Строгий → старый strict-путь (без SRC). Старые перечисления — только внутренние параметры пути. Тесты ≤4 (по одному на режим + Optimal без Shared-фолбэка). Проверка: cargo test engine::legacy_path
- [x] Шаг 11: src/persist/settings_file.rs: новое поле Settings.modes: ModeSettings — раздел [playback] через PlaybackDto (ошибка типа в разделе → load_mode_settings(None) + заметка Invalid, 02 spec стр. 2364), запись через playback_dto последним разделом (02 §2.4/§2.6); [audio], [dsd], audio_device больше не читаются и не пишутся (неизвестные ключи с заметкой); поле playback: LegacyPlayback временно остаётся мостом только в памяти (default). Тесты файла с [audio] обновить; новый: старый файл → modes = default и заметки. Проверка: cargo test persist::settings_file
- [x] Шаг 12: src/persist/state_file.rs: LegacyPlaybackState → PlaybackState (ключи playback.active, playback.compatible.volume/muted, playback.optimal.volume/muted, playback.strict.muted — 02 spec §2.5 стр. 1238–1243); StateChange::Volume/Muted применяются к активному режиму (Volume в Строгом — без эффекта, ТЗ-41), новый StateChange::ActiveMode(ModeKind); геттеры громкости/mute активного режима для текущих потребителей. Тесты: круговой разбор, Volume в Строгом игнорируется. Проверка: cargo test persist::state_file; ЧЕКПОИНТ: cargo test + clippy
- [x] Шаг 13: src/engine/messages.rs + src/engine/run.rs: EngineCmd::SetModeSettings(ModeSettingsUpdate) и SetActiveMode(ModeKind) (SetLegacyAudio пока остаётся мостом); движок хранит ModeSettings + active и строит параметры пути через legacy_path; diff None → только копия; Some → по strongest (§6.18): Memory → копия; ReopenAtPosition → одно переоткрытие с позиции с сохранением playing/paused (stopped → только копия); SwitchDevice → смена устройства с позиции. Проверка: cargo check
- [x] Шаг 14: src/engine/run.rs + src/engine/tests.rs: вынести решение применения в чистую fn mode_apply_action(diff: Option<&ModeSettingsDiff>, active, stopped) -> ModeApplyAction {Store, ReopenAtPosition, SwitchDevice} (§6.18, И-Р26) и тесты: изменение параметра активного режима → ReopenAtPosition; изменение неактивного режима → Store; device + другой параметр → SwitchDevice (поглощение); stopped → Store. Тест audio_param_change_reopens_at_position_preserving_pause (§7.2 стр. 1834) с реальным переоткрытием невозможен без фейка Player (мост С3, ТЗ-114) — переносится на этап с SignalPath (заметка плана). Проверка: cargo test engine::
- [x] Шаг 15: src/engine/run.rs (+ src/engine/tests.rs): правило громкости — действующий gain 1.0 в Строгом и в Оптимальном с volume_lock, SetVolume там без эффекта; mute работает во всех режимах (§6.18, ОВС-18…20, ADR-23). Тест optimal_volume_lock_uses_nogain (§7.2 стр. 2916). Проверка: cargo test engine::; ЧЕКПОИНТ: cargo test + clippy
- [x] Шаг 16: BackendCaps { exclusive } (§2.3 стр. 920): exclusive = cfg!(target_os = "linux") (ОВС-12, ТЗ-109, ТЗ-110), BackendCaps::available(mode) = mode == Compatible || exclusive; EngineHandle::spawn -> Result<(EngineHandle, BackendCaps), EngineFault>; вызовы в src/main.rs и тестах. Файлы: src/engine/run.rs, src/main.rs. Проверка: cargo check
- [>] **Шаг 17: src/engine/run.rs + src/engine/tests.rs: Open в недоступном режиме → OpenFailed(ModeUnavailable) без воспроизведения (§6.18 п. 2). Тесты modes_availability_from_backend_caps, saved_unavailable_mode_no_playback (§7.2 стр. 2863–2864). Проверка: cargo test engine::**
- [ ] Шаг 18: src/app/audio_facade.rs: вместо SetLegacyAudio отправлять SetModeSettings / SetActiveMode (источник — Settings.modes и PlaybackState); громкость и mute — активного режима. Проверка: cargo check
- [ ] Шаг 19: Снять мост SetLegacyAudio: удалить вариант из src/engine/messages.rs и обработку в src/engine/run.rs; LegacyAudio остаётся внутренним типом движка. Проверка: cargo check; ЧЕКПОИНТ: cargo test + clippy
- [ ] Шаг 20: Старт: до первого Open отправить SetModeSettings{diff: None} и SetActiveMode(сохранённый режим) (§6.27, ОВС-14). Файлы: src/app/mod.rs, src/app/audio_facade.rs. Тест startup_policy_equals_saved_policy (стр. 549). Проверка: cargo test startup_policy
- [ ] Шаг 21: Slint: переключатель трёх режимов в главном окне (ui/top_panel.slint + проброс в ui/app.slint): свойства active-mode, mode-available[3] с причиной, callback select-mode(int); недоступный режим неактивен с пояснением (ТЗ-18, ТЗ-109, ТЗ-110, §8 С4). Мост: обработчик Rust — следующий шаг. Проверка: cargo check
- [ ] Шаг 22: Rust: обработчик select-mode — StateChange::ActiveMode + SetActiveMode + Open текущего трека с позиции (§6.18 «Смена режима», ТЗ-20); синхронизация active-mode/available из BackendCaps; ползунок громкости показывает громкость активного режима, неактивен в Строгом и при volume_lock. Файлы: src/app/mod.rs, src/app/playback_manager.rs. Проверка: cargo check
- [ ] Шаг 23: src/core/testing.rs: тесты mode_settings_preserved_across_switch_and_restart (стр. 2886) и load_correction_not_written_until_save (стр. 2922). Проверка: cargo test core::; ЧЕКПОИНТ: cargo test + clippy
- [ ] Шаг 24: src/app/mode_dialog.rs (новый, `mod mode_dialog` в src/app/mod.rs): чистый построитель строк диалога для режима из PARAMS/Availability и ModeSettings: подпись, вид (список/переключатель/ползунок), варианты с запретом и подсказкой (buffers_allowed для двух буферов, min-phase SrcFilter и «ремодуляция» как NotImplemented), FixedByMode — значение и «задано режимом», Unavailable — причина (§6.27, ТЗ-95, ТЗ-128…131, И-Т19). Тесты ≤5. Проверка: cargo test mode_dialog
- [ ] Шаг 25: src/app/mode_dialog.rs: apply_edit(&mut ModeSettings, ModeKind, ParamId, значение) — изменение черновика из строки диалога; недопустимое сочетание буферов не применяется (ТЗ-94). Тесты ≤4. Проверка: cargo test mode_dialog
- [ ] Шаг 26: Slint: ui/mode_params.slint (новый) — компонент панели режимов: выбор редактируемого режима, строки по виду (список/переключатель/ползунок, неактивные с причиной/«задано режимом»/«пока не реализовано»); подключить во вкладку «Аудио» ui/settings.slint рядом со старым блоком (мост) с пробросом в ui/app.slint. Проверка: cargo check
- [ ] Шаг 27: Rust: связать панель режимов — черновик Settings.modes через apply_edit, синхронизация строк; «Сохранить» → ModeSettings::diff → SetModeSettings(Some(diff)) только при непустом diff, запись в настройки (ОВС-14, §6.18). Файлы: src/app/mode_dialog.rs, src/app/mod.rs. Проверка: cargo check; ЧЕКПОИНТ: cargo test + clippy
- [ ] Шаг 28: Удалить старые Rust-обработчики аудио-диалога: on_settings_set_audio_*/set_dsd_mode/set_bit_perfect и resampler_fixed_fallback_guard с тестом (src/app/mod.rs), sync_capabilities_and_validation/sync_dsd_chain_desc/sync_audio_advanced (src/app/ui_manager.rs) (§8 С4 «удаляется»). Проверка: cargo check
- [ ] Шаг 29: Удалить старые аудио-свойства и колбэки из ui/settings.slint и проброс из ui/app.slint (bit-perfect, dsd-mode, dsd-bp-warn, audio-validation, audio-exclusive*, audio-fallback*, audio-resampler-mode-idx, audio-fixed-rate-idx, audio-clock-family-idx, audio-ring-buffer-ms, audio-dsd-chain-desc, audio-fixed-fail-warn, set-*-соответствующие). Проверка: cargo check
- [ ] Шаг 30: src/audio/output.rs: удалить validate_audio_settings, ValidationRow/Outcome при отсутствии других потребителей и их тесты (§8 С4 «удаляется»). Проверка: cargo check; ЧЕКПОИНТ: cargo test + clippy
- [ ] Шаг 31: Снять мост Settings.playback: LegacyPlayback: удалить поле, Settings.modes → Settings.playback: ModeSettings (02 spec §2.4 стр. 1072); остальные читатели — через legacy_path или PlaybackState. Файлы: src/persist/settings_file.rs и оставшиеся читатели (по cargo check). Проверка: cargo check
- [ ] Шаг 32: src/settings.rs: старые перечисления (ExclusiveMode, FallbackPolicy, ResamplerMode, FallbackRatePolicy, ClockFamily, DsdMode, TargetBitDepth, TargetSampleRate, ResamplerDither::Triangular, AudioCfg.bit_perfect) перестают быть пользовательскими настройками: убрать serde и UI-индексы, удалить их toml-тесты; Triangular — удалить (§8 С4, ОВС-12). Проверка: cargo check; ЧЕКПОИНТ: cargo test + clippy
- [ ] Шаг 33: Бейдж: «Bit-perfect» недостижим до С6 — старый путь не читает hw_params (DriverFormat::Unreadable): src/app/bp_report.rs / src/app/playback_manager.rs никогда не показывают Bit-perfect; тест (§8 С4, ТЗ-52). Проверка: cargo test bp_report
- [ ] Шаг 34: Финал: cargo build/test/clippy зелёные; ROADMAP ✅; заметка плана; напомнить пользователю удалить settings.toml (ТЗ-97) и показать расхождение с 02 spec стр. 3342 (решение — за пользователем); ручные проверки (переключатель режимов, Совместимый играет, Оптимальный/Строгий через hw: на Linux). Проверка: cargo test + clippy

Легенда: [x] сделано · [>] текущий шаг · [ ] не начато

- **Текущий шаг (current_step):** Шаг 17
- **Следующий ход:** Шаг 17: Open в недоступном режиме → OpenFailed(ModeUnavailable) + тесты
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

- [~] **7.** Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче
  - Инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
- [>] **13.** Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА
  - Закрытые этапы и баги — в ROADMAP.md (✅ с хэшем); здесь только открытое.
  - SP1.0-8.8 (С8 (02) «Загрузка плейлиста, Play Now, экспорт») закрыт в 5b08e7b; В РАБОТЕ (п. 14 сквозного порядка): AM1.0-8.4 (С4 (01) «Модель режимов и настроек») — микро-шаги в _STATE_.yaml.
  - ВОПРОС ПОЛЬЗОВАТЕЛЮ (AM1.0-8.4 шаг 2): HwDeviceId (§2.3) ещё нет в коде; в OptimalOpts/StrictOpts.device временно Option<String>. Предложение: добавить newtype HwDeviceId в src/audio/backend/mod.rs (расширить вайтлист) и заменить. Умолчание BufferMs = 1500 мс (как RING_BUFFER_MS_DEFAULT; спека числа не задаёт).
  - ВОПРОС ПОЛЬЗОВАТЕЛЮ (AM1.0-8.4 шаг 10): legacy_path отображает SrcFilter на старый ResamplerAlgorithm по крутизне: Steep→SincSlow(128), Slow→SincMedium(64), VerySlow→SincFast(32) — решение шага, в спеке нет; Оптимальный → ExclusiveMode::Strict (без перехода в Shared); DsdFilter/dsd_gain_comp/volume_lock/device_buffer старый путь не параметризуют.
  - AM1.0-8.4 шаг 13: SetActiveMode в движке только сохраняет параметры (release_engine + поля), переоткрытие — последующий Open от UI (§6.18 стр. 2399, ТС-9). Пробел: при возврате на SystemDefault (legacy_audio → device None) Player.preferred_device не сбрасывается — нет API в player.rs (вне вайтлиста); закрыть на С5 или расширить вайтлист.
  - AM1.0-8.4 шаг 14: тест audio_param_change_reopens_at_position_preserving_pause (§7.2 стр. 1834) не написан — Engine владеет настоящим Player (Decoder + cpal), фейка нет (ТЗ-114); проверено только решение mode_apply_action. Дописать, когда путь станет фейкуемым через EngineDeps (С5+).
  - AM1.0-8.4 шаг 7: имена ключей [playback] dither (tpdf/off), rate_fallback (same_family/nearest/no_downsample), volume_lock, fixed_rate (Гц), buffer (мс), device (строка) спекой поимённо не заданы — взяты по именам полей ModeSettings; подтвердить пользователю.
  - На С5 (решение пользователя 2026-10-08): снять мост src/app/audio_facade.rs — вызовы в playback_manager, visualizer_manager, playlist_manager, bp_report, mod.rs → прямая работа с EngineHandle/SignalPath/BadgeState; удалить файл; снять #![allow(dead_code)] в engine_sink.rs и ui_audio_state.rs. Состояние фасада (req_gen, кэш транспорта/трека/потока, ended, очередь резервирования, volume/muted/LegacyAudio) переносить целиком, не по файлам.
  - Ручные проверки за пользователем: AM1.0-8.1 — сценарии ТЗ-1/2/48/118/119/120/122 — отложено до полной реализации замка (решение пользователя 2026-10-10).
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
