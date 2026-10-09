<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** SP1.0-8.7 — С7 (02). Выбор файлов и ввод-вывод вне UI
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/platform/mod.rs
  - src/platform/pick/mod.rs
  - src/platform/pick/fake.rs
  - src/platform/pick/rfd.rs
  - src/cover.rs
  - src/audio/fulltrack.rs
  - src/core/mod.rs
  - src/core/io.rs
  - src/core/testing.rs
  - src/main.rs
  - src/app/mod.rs
  - src/app/ui_manager.rs
  - src/app/fulltrack_manager.rs
  - ROADMAP.md
  - tools/state_tool.py
- **Критерий успеха (Definition of Done):** FilePicker/RfdPicker/FakePicker (§2.8, ADR-10); поток apap-io с IoJob/IoDone и FakeIo (§2.12, ADR-20); выбор файлов не блокирует цикл событий, шлюз держит причину FilePicker (ТЗ-53); размер кэшей считается при открытии диалога и после очистки, не на тике; очистка и чтение тем вне UI-потока (ТЗ-22, ТЗ-34); удалены rfd::FileDialog, sync_cache_stats_to_ui на тике, чтение тем в UI-потоке; cargo test + clippy зелёные; плеер играет в Совместимом режиме

## Итерационный трекер
- [x] Шаг 1: pick/mod.rs: PickRequest {AddFiles, AddFolder, OpenPlaylist, SavePlaylist, ThemeFile} {start: Option<PathBuf>}, PickResult {Paths, Cancelled}, трейт FilePicker::pick(&self, req, parent: Option<&slint::Window>) -> Pin<Box<dyn Future<Output=PickResult>>> (§2.8, ADR-10, ТЗ-53, ТЗ-54 п.5). Файлы: src/platform/pick/mod.rs (новый), src/platform/mod.rs. Проверка: cargo check
- [x] Шаг 2: pick/fake.rs: FakePicker — ответ заданными путями, Cancelled или «никогда» (std::future::pending), счётчик вызовов; тест platform_fakes_picker (§7.1, ADR-10). Файлы: src/platform/pick/fake.rs (новый), src/platform/pick/mod.rs. Проверка: cargo test platform::pick
- [x] Шаг 3: pick/rfd.rs: RfdPicker на rfd::AsyncFileDialog — фильтры (аудио; m3u/m3u8; сохранение с .m3u8 по умолчанию, ОВ-17; тема toml), set_directory(start), set_parent(window_handle) (ADR-10, ТЗ-53). Файлы: src/platform/pick/rfd.rs (новый), src/platform/pick/mod.rs. Проверка: cargo check
- [x] Шаг 4: Публичные операции кэша над заданным каталогом: cover_cache_size_in/clear_cover_cache_in, disk_cache_size_in/clear_disk_cache_in → pub (нужны apap-io с инжектируемыми путями, ADR-20). Файлы: src/cover.rs, src/audio/fulltrack.rs. Проверка: cargo check
- [x] Шаг 5: core/io.rs: IoJob {ReadTheme, ListThemes (ОТКЛОНЕНИЕ: список тем при открытии диалога), CacheSizes, ClearCache}, IoDone {Theme, Themes, CacheSizes, Cleared}, CacheKind {Visualization, Covers, All}, CacheSizes {viz_disk, covers}, трейт IoWorker {submit, try_recv} (§2.12, ADR-20, ТЗ-22). Файлы: src/core/io.rs (новый), src/core/mod.rs. Проверка: cargo check
- [x] Шаг 6: core/io.rs: spawn_io(IoPaths{themes, covers, viz}) — поток apap-io, два mpsc, поколение возвращается в ответе; тесты на временных каталогах: чтение темы, подсчёт размеров, очистка (ADR-20, ТЗ-22, ТЗ-34). Файл: src/core/io.rs. Проверка: cargo test core::io
- [x] Шаг 7: ЧЕКПОИНТ: cargo test + cargo clippy (фильтр по src/platform/pick, src/core/io.rs, src/cover.rs, src/audio/fulltrack.rs). Файлы: —. Проверка: зелёные, 0 новых варнингов
- [x] Шаг 8: FakeIo в core/testing.rs: синхронные ответы IoDone, счётчики заданий по виду; тест platform_fakes_io (§7.1). Файл: src/core/testing.rs. Проверка: cargo test core::testing
- [x] Шаг 9: MusicApp: поле io: Box<dyn IoWorker>, поколения по виду задания; main строит spawn_io с путями themes/covers/viz; drain_io на тике (пока без применения ответов) (ADR-20, §4). Файлы: src/main.rs, src/app/mod.rs. Проверка: cargo check
- [x] Шаг 10: UI размеров кэша без обхода диска: fulltrack_manager cache_sizes → только RAM; ui_manager sync_cache_stats_to_ui → apply_cache_sizes(&CacheSizes) (RAM из памяти, диск из ответа apap-io) (ТЗ-22). Файлы: src/app/ui_manager.rs, src/app/fulltrack_manager.rs. Проверка: cargo check
- [x] Шаг 11: app/mod.rs: открытие диалога → io.submit(CacheSizes{gen}) один раз; убрать пересчёт на тике; drain_io применяет CacheSizes/Cleared по поколению (ТЗ-22, §6.9). Файл: src/app/mod.rs. Проверка: cargo check
- [x] Шаг 12: app/mod.rs: очистка кэшей (виз./обложки/всё) → RAM-кэш очищается в UI, диск — io.submit(ClearCache{which}); ответ Cleared → новый размер, без сообщений (ТЗ-34, §6.9). Файлы: src/app/mod.rs, src/app/ui_manager.rs. Проверка: cargo check. Мост sync_cache_stats_to_ui удалён из ui_manager.rs — вызовов не осталось.
- [x] Шаг 13: app/mod.rs: темы при открытии диалога → ListThemes + ReadTheme(текущая) через apap-io, ответы заполняют список/метаданные (убрать scan_themes_dir и load_theme_meta из UI-потока) (ТЗ-22, ADR-20). Файл: src/app/mod.rs. Проверка: cargo check
- [x] Шаг 14: app/mod.rs: выбор темы → ReadTheme{gen}; прочитанные данные Arc<ThemeData> хранятся с выбором; «Сохранить» применяет их без повторного чтения файла (убрать resolve_startup_theme из обработчика сохранения) (ТЗ-22, ТЗ-29, ADR-20). Файл: src/app/mod.rs. Проверка: cargo check
- [x] Шаг 15: ЧЕКПОИНТ: cargo test + cargo clippy (фильтр по src/app/, src/main.rs, src/core/). Файлы: —. Проверка: зелёные, 0 новых варнингов
- [x] Шаг 16: MusicApp: поле picker: Box<dyn FilePicker> (main → RfdPicker); помощник pick_async(app, req, on_paths): gate.block(FilePicker) → slint::spawn_local(future) → unblock → LastDir с Origin::User → колбэк; повторный выбор отклоняет шлюз (ADR-10, ADR-12, ТЗ-53). Файлы: src/main.rs, src/app/mod.rs. Проверка: cargo check. Обработчик add_files переведён на pick_async в этом же шаге (без мёртвого кода).
- [x] Шаг 17: Заменить блокирующие rfd::FileDialog в add_folder/load_playlist на pick_async (add_files уже переведён в шаге 16); удалить use rfd::FileDialog (ТЗ-53). Файл: src/app/mod.rs. Проверка: cargo check; grep rfd::FileDialog пуст
- [>] **Шаг 18: Тест menu_inactive_while_picker_open: FakePicker «никогда» + UiGate(FilePicker) — команды меню отклонены, второй выбор не открыт (§7.2, ТЗ-23, ТЗ-53). Файл: src/core/testing.rs. Проверка: cargo test menu_inactive_while_picker_open**
- [ ] Шаг 19: ЧЕКПОИНТ финальный: cargo test + cargo clippy полностью; grep: нет rfd::FileDialog, sync_cache_stats_to_ui на тике, ThemeData::load_from_file/scan_themes_dir в обработчиках UI. Файлы: —. Проверка: зелёные, 0 новых варнингов

Легенда: [x] сделано · [>] текущий шаг · [ ] не начато

- **Текущий шаг (current_step):** Шаг 18
- **Следующий ход:** Шаг 18: новый субагент — тест menu_inactive_while_picker_open (src/core/testing.rs)
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

- [~] **7.** Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче
  - Инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
- [>] **13.** Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА
  - Закрытые этапы и баги — в ROADMAP.md (✅ с хэшем); здесь только открытое.
  - ТЕКУЩИЙ: SP1.0-8.7 (С7 (02) «Выбор файлов и ввод-вывод вне UI») — 🔄, микро-шаги в _STATE_.yaml; SP1.0-8.6 закрыт в 091b0b2.
  - Ручные проверки за пользователем: SP1.0-8.6 — cargo run: щелчок по заголовку Asc→Desc→без ключа (стрелка исчезает), «Сейчас играет» не сортируется; сортировка не меняет mtime playlist.m3u; ключ сортировки восстанавливается после перезапуска; Shuffle проигрывает каждый трек один раз, «Назад» идёт по истории; «Далее» без текущего — первый видимый.
  - Ручные проверки за пользователем: AM1.0-8.3 — UI не блокируется при открытии/смене трека и устройства; ошибки треков видны; Exclusive через старый cpal-путь; выход ≤ 2 с (выход подтверждён 2026-10-08).
  - На С5 (решение пользователя 2026-10-08): снять мост src/app/audio_facade.rs — вызовы в playback_manager, visualizer_manager, playlist_manager, bp_report, mod.rs → прямая работа с EngineHandle/SignalPath/BadgeState; удалить файл; снять #![allow(dead_code)] в engine_sink.rs и ui_audio_state.rs. Состояние фасада (req_gen, кэш транспорта/трека/потока, ended, очередь резервирования, volume/muted/LegacyAudio) переносить целиком, не по файлам.
  - Ручные проверки за пользователем: AM1.0-8.1 — сценарии ТЗ-1/2/48/118/119/120/122.
  - Ручные проверки за пользователем: SP1.0-8.2 — меню при открытом диалоге/окне сообщения, Enter/Esc, Совместимый режим.
  - Ручные проверки за пользователем: SP1.0-8.3/8.4 — cargo run; закрытие окна и выход из трея сохраняют геометрию, плейлист, громкость.
  - Ручные проверки за пользователем: SP1.0-B5/B6 — «Файл → Удалить выделенный трек» / «Очистить плейлист»; при открытых «Параметрах» пункты неактивны.
  - Геометрия окна под Wayland проверена пользователем 2026-10-06; X11 не проверяется (решение пользователя).
  - Отклонения старого пути до С6: повтор EBUSY на тике 100 мс; select_output_for может кратко пробовать hw: до резервирования.
  - На С6: баг AM1.0-B1 (устройство не возвращается в PipeWire после hw:, EBUSY при пересоздании узла); тест unreadable_playlist_never_written (в AppCore нет чтения плейлиста) — по §8 стартовое чтение в AppCore приходит в С8 (02), не в С6 (02).
  - На С9 (с переносом диалога и шлюза в AppCore): тесты picker_does_not_block_loop, cache_size_counted_once_per_dialog, cache_clear_survives_cancel (на С7 нет dialog_open/gate в AppCore); perf_picker_open_30s — на С12.
  - VizCycle (viz_settings_manager.rs) защищён только Slint-оверлеем.
  - Ручные проверки за пользователем: SP1.0-8.5 — SIGTERM/SIGINT/SIGHUP (kill) сохраняют state/settings, второй сигнал завершает сразу; выход из трея; уведомление при ошибке записи с окном в трее.
  - На Windows-ноутбуке/macOS: собрать и проверить SP1.0-8.5 (WM_ENDSESSION-сабкласс windows.rs, applicationShouldTerminate: macos.rs, отсутствие трея) — локально не компилировалось.
  - Строка tray_works_during_dialog: проверка SetModeSettings и колёсика — на С11/с модулем режимов.
  - SP1.0-B5 вынес из С9 перенос кнопок плейлиста в меню; на С9 остаётся остальное. Текст ТЗ-34 «Удалить текущий трек» расходится с реализацией (выделенный) — правка docs по разрешению пользователя.
  - На Windows-ноутбуке: проверить cargo build (Windows/macOS проверены только ревью).
  - На С12: ручной strace ТЗ-19.

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
