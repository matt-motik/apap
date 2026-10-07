<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->


# Текущая микро-сессия

- **Задача из ROADMAP:** SP1.0-8.5 — С5. Жизненный цикл, трей, уведомления (ТЗ-14, 15, 24, 52 п.2, 54 п.1, п.4)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - Cargo.toml
  - Cargo.lock
  - ROADMAP.md
  - _STATE_.yaml
  - _STATE_.md
  - src/lib.rs
  - src/main.rs
  - src/tray.rs
  - src/platform/mod.rs
  - src/platform/tray/mod.rs
  - src/platform/lifecycle/mod.rs
  - src/platform/lifecycle/fake.rs
  - src/platform/lifecycle/unix.rs
  - src/platform/lifecycle/windows.rs
  - src/platform/lifecycle/macos.rs
  - src/platform/notify/mod.rs
  - src/platform/notify/fake.rs
  - src/platform/notify/none.rs
  - src/platform/notify/linux.rs
  - src/core/exit.rs
  - src/journal.rs
  - src/core/mod.rs
  - src/core/messages.rs
  - src/core/testing.rs
  - src/app/mod.rs
- **Критерий успеха (Definition of Done):** Все пути выхода (окно, трей, SIGTERM/INT/HUP, сеанс Windows, macOS terminate) идут в единый путь выхода через модуль жизненного цикла; второй сигнал завершает сразу; трей только Linux; уведомления через Notifier; тесты §7.2 для С5 зелёные; cargo test/clippy без новых варнингов; cfg(target_os) для задач SP1.0 только в src/platform

## Итерационный трекер
[x] Шаг 1: ExitReason: варианты Signal(TermSignal), WindowsSessionEnd, MacosTerminate + enum TermSignal (§2.8, ТЗ-14, ТЗ-15). Файлы: src/core/exit.rs, src/journal.rs (exit_reason_text; + src/app/mod.rs только если match неисчерпывающий). Проверка: cargo check
[x] Шаг 2: exit() в app: для WindowsSessionEnd/MacosTerminate — Completed без slint::quit_event_loop (ADR-7, §6.10, ТЗ-15). Файл: src/app/mod.rs. Проверка: cargo check
[x] Шаг 3: Трейт Notifier + NoneNotifier (ADR-9, §2.8, ТЗ-54 п.4). Файлы: src/platform/notify/mod.rs, src/platform/notify/none.rs. Проверка: cargo check
[x] Шаг 4: FakeNotifier — записывает вызовы, всегда компилируется (ADR-6, ADR-9). Файл: src/platform/notify/fake.rs. Проверка: cargo check
[x] Шаг 5: Запись журнала JournalRecord::Notify { error } + текст строки (§2.9, ADR-9, ADR-21). Файл: src/journal.rs. Проверка: cargo check
[x] Шаг 6: LinuxNotifier (cfg linux): tokio Handle потока трея + Arc<dyn Journal>; notify() спавнит вызов org.freedesktop.Notifications.Notify через zbus, ошибка → JournalRecord::Notify (ADR-9, ТЗ-52 п.2). Файлы: src/platform/notify/linux.rs, src/platform/notify/mod.rs (только объявление модуля). Проверка: cargo check
[x] Шаг 7: Фабрика platform_notifier(Option<Handle>, journal) в notify/mod.rs: Linux+Handle → LinuxNotifier, иначе NoneNotifier (ADR-6, ADR-9, ТЗ-54 п.4). Файл: src/platform/notify/mod.rs. Проверка: cargo check. ЧЕКПОИНТ: cargo test + cargo clippy
[x] Шаг 8: Типы жизненного цикла: TrayEvent, TrayScroll, PlatformError, ExitEntry, ProcessExit, трейт Lifecycle (§2.8, ADR-7, ТЗ-54 п.1). Файл: src/platform/lifecycle/mod.rs. Проверка: cargo check
[x] Шаг 9: FakeLifecycle: caps задаёт тест, fire(ExitReason) вызывает ExitEntry (ADR-6, §2.8). Файл: src/platform/lifecycle/fake.rs. Проверка: cargo check
[x] Шаг 10: Поток apap-signals: tokio current_thread + signal (SIGTERM/INT/HUP) будит цикл событий; второй сигнал → ProcessExit(128+signo) (ADR-7, ТЗ-14, ТЗ-15). Файлы: src/platform/lifecycle/unix.rs, Cargo.toml (tokio feature signal). Проверка: cargo check
[x] Шаг 11: Тест double_signal_exits_immediately (подменный ProcessExit, задержка записи 10 с, два SIGTERM → 143 сразу) (§7.2, ТЗ-14). Файл: src/platform/lifecycle/unix.rs. Проверка: cargo test double_signal. ЧЕКПОИНТ: cargo test + cargo clippy
[x] Шаг 12: Перенос src/tray.rs → src/platform/tray/mod.rs (git mv) с мостом `pub use platform::tray;` в lib.rs (§8.1 С5). Файлы: src/lib.rs, src/platform/mod.rs (+ перемещённый файл). Проверка: cargo check
[ ] Шаг 13: Импорты app/main на crate::platform::tray, удалить мост из lib.rs (§8.1 С5). Файлы: src/app/mod.rs, src/lib.rs (main.rs — если импортирует tray). Проверка: cargo check
[ ] Шаг 14: Трей шлёт TrayEvent вместо TrayCmd; Wheel → Scroll(TrayScroll), в poll_tray прежний шаг громкости до С11 (§2.8, ТЗ-24). Файлы: src/platform/tray/mod.rs, src/app/mod.rs. Проверка: cargo check
[ ] Шаг 15: ksni — Linux target dep; реализация трея под cfg внутри platform/tray, на прочих ОС start() → None (ADR-6, В-1). Файлы: Cargo.toml, src/platform/tray/mod.rs. Проверка: cargo check. ЧЕКПОИНТ: cargo test + cargo clippy
[ ] Шаг 16: Реальные PlatformCaps: tray = хост StatusNotifier найден, notifications = tray (вместо допущения) (§2.8, ADR-6, ТЗ-52 п.2). Файлы: src/platform/tray/mod.rs, src/app/mod.rs. Проверка: cargo check
[ ] Шаг 17: UnixLifecycle: impl Lifecycle (install запускает apap-signals) + фабрика платформенной реализации в lifecycle/mod.rs (ADR-7, ADR-23 шаг 8). Файлы: src/platform/lifecycle/unix.rs, src/platform/lifecycle/mod.rs. Проверка: cargo check
[ ] Шаг 18: main.rs: ExitEntry (try_borrow_mut → Busy) и lifecycle.install после показа окна (ADR-23 шаг 8, ТЗ-14). Файл: src/main.rs. Проверка: cargo check
[ ] Шаг 19: Запуск трея перенести из MusicApp::new в Lifecycle::install (после сигналов; ADR-23 шаг 8). Файлы: src/app/mod.rs, src/platform/lifecycle/unix.rs. Проверка: cargo check. ЧЕКПОИНТ: cargo test + cargo clippy
[ ] Шаг 20: TrayEvent::Quit → тот же ExitEntry(TrayQuit); удалить отдельный путь выхода в poll_tray (§8.1 «удаляется», ТЗ-14). Файл: src/app/mod.rs. Проверка: cargo check
[ ] Шаг 21: on_close_requested: сворачивание в трей только при minimize_to_tray && caps.tray, иначе exit(WindowClose) (§6.10, ТЗ-52 п.2, В-1). Файл: src/app/mod.rs. Проверка: cargo check
[ ] Шаг 22: apply_msg_effect: notify через Notifier вместо set_tray_notice; Notifier внедряется из main (§6.15, ADR-9, ТЗ-52 п.2). Файлы: src/app/mod.rs, src/main.rs. Проверка: cargo check
[ ] Шаг 23: Удалить BP_NOTICE_TEXT/notice как канал сообщений (§8.1 «удаляется»). Файлы: src/platform/tray/mod.rs, src/app/mod.rs. Проверка: cargo check. ЧЕКПОИНТ: cargo test + cargo clippy
[ ] Шаг 24: Windows: SetWindowSubclass на HWND (WM_QUERYENDSESSION→TRUE; WM_ENDSESSION wParam=TRUE → синхронный выход, return 0) (ADR-7, ТЗ-15). Файлы: src/platform/lifecycle/windows.rs, Cargo.toml (windows-sys features). Проверка: cargo check (Linux) + ревью; сборка Windows — на ноутбуке
[ ] Шаг 25: macOS: applicationShouldTerminate: через class_addMethod на делегат winit, фолбэк NSApplicationWillTerminateNotification (ADR-7, ТЗ-15). Файлы: src/platform/lifecycle/macos.rs, Cargo.toml (objc2*). Проверка: cargo check (Linux) + ревью
[ ] Шаг 26: Тесты выхода: exit_writes_settings_once (по каждой причине; без TrayQuit без трея), repeated_tray_quit_ignored, windows_session_end_runs_exit_synchronously (§7.2, ТЗ-14, ТЗ-15). Файл: src/core/testing.rs. Проверка: cargo test --no-run (прогон группой на Шаге 28)
[ ] Шаг 27: Тесты сообщений: tray_hidden_error_notifies_once, no_tray_error_shows_window, tray_quit_closes_error_window_one_attempt (§7.2, ТЗ-52). Файл: src/core/testing.rs. Проверка: cargo test --no-run (прогон группой на Шаге 28)
[ ] Шаг 28: Тесты: platform_fakes_cover_exit_fs_input_notify_pick (части exit/fs/notify), tray_works_during_dialog/message без колёсика (колёсико — С11) (§7.2, ТЗ-24, ТЗ-54). Файл: src/core/testing.rs. Проверка: ЧЕКПОИНТ группы тестов 26–28: полный cargo test + cargo clippy
[ ] Шаг 29: Финал: grep cfg(target_os вне src/platform (аудио-места AM1.0 — вне задач SP1.0), полный cargo test + cargo clippy, ROADMAP ✅ (ТЗ-54 приёмка). Файлы: ROADMAP.md, _STATE_.yaml. Проверка: всё зелёное

- **Текущий шаг (current_step):** Шаг 13
- **Следующий ход:** Шаг 13: импорты на crate::platform::tray, удалить мост
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

- [~] **7.** Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче
  - Инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
- [>] **11.** Реализация AM1.0 + SP1.0 по сквозному порядку (ROADMAP.md): docs/01_audio_modes_v1.0/ (С0…С11) и docs/02_settings_persistence_v1.0/ (С0…С12) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА
  - Закрытые этапы и баги — в ROADMAP.md (✅ с хэшем); здесь только открытое.
  - СЛЕДУЮЩИЙ: SP1.0-8.5 (С5 (02) «Жизненный цикл, трей, уведомления»), потом AM1.0-8.3. Старт этапа — AGENTS.md Шаги 2–3: строка этапа §8 и все относящиеся разделы (ТЗ-N, ADR-N, §2–§3, §5, §6, §7).
  - Ручные проверки за пользователем: AM1.0-8.1 — сценарии ТЗ-1/2/48/118/119/120/122.
  - Ручные проверки за пользователем: SP1.0-8.2 — меню при открытом диалоге/окне сообщения, Enter/Esc, Совместимый режим.
  - Ручные проверки за пользователем: SP1.0-8.3/8.4 — cargo run; закрытие окна и выход из трея сохраняют геометрию, плейлист, громкость.
  - Ручные проверки за пользователем: SP1.0-B5/B6 — «Файл → Удалить выделенный трек» / «Очистить плейлист»; при открытых «Параметрах» пункты неактивны.
  - Геометрия окна под Wayland проверена пользователем 2026-10-06; X11 не проверяется (решение пользователя).
  - Отклонения старого пути до С6: повтор EBUSY на тике 100 мс; select_output_for может кратко пробовать hw: до резервирования.
  - На С6: баг AM1.0-B1 (устройство не возвращается в PipeWire после hw:, EBUSY при пересоздании узла); тест unreadable_playlist_never_written (в AppCore нет чтения плейлиста).
  - До С5 notify идёт подсказкой трея; VizCycle (viz_settings_manager.rs) защищён только Slint-оверлеем.
  - SP1.0-B5 вынес из С9 перенос кнопок плейлиста в меню; на С9 остаётся остальное. Текст ТЗ-34 «Удалить текущий трек» расходится с реализацией (выделенный) — правка docs по разрешению пользователя.
  - На Windows-ноутбуке: проверить cargo build (Windows/macOS проверены только ревью).
  - На С12: ручной strace ТЗ-19.

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено
