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
  - SP1.0-8.8 (С8 (02) «Загрузка плейлиста, Play Now, экспорт») закрыт в 5b08e7b; СЛЕДУЮЩИЙ (п. 14 сквозного порядка): AM1.0-8.4 (С4 (01) «Модель режимов и настроек») — сформировать микро-шаги в _STATE_.yaml.
  - На С5 (решение пользователя 2026-10-08): снять мост src/app/audio_facade.rs — вызовы в playback_manager, visualizer_manager, playlist_manager, bp_report, mod.rs → прямая работа с EngineHandle/SignalPath/BadgeState; удалить файл; снять #![allow(dead_code)] в engine_sink.rs и ui_audio_state.rs. Состояние фасада (req_gen, кэш транспорта/трека/потока, ended, очередь резервирования, volume/muted/LegacyAudio) переносить целиком, не по файлам.
  - Ручные проверки за пользователем: AM1.0-8.1 — сценарии ТЗ-1/2/48/118/119/120/122 — отложено до полной реализации замка (решение пользователя 2026-10-10).
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
