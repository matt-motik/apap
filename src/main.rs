#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use app::MusicApp;
use music_player_rs::audio::clock::MonotonicClock;
use music_player_rs::core::exit::{ChannelWaiter, ExitOutcome, ExitReason};
use music_player_rs::core::{AppCore, AppDeps, NoEngine};
use music_player_rs::engine::deps::EngineDeps;
use music_player_rs::engine::run::EngineHandle;
use music_player_rs::journal::{FileJournal, Journal};
use music_player_rs::persist::keys::Parsed;
use music_player_rs::persist::writer::{spawn_writer, WriterCmd, WriterHandle, WriterReply};
use music_player_rs::persist::{self, BadCopyOutcome, Boot, ConfigFile, ConfigPaths};
use music_player_rs::platform::fs::os_fs;
use music_player_rs::platform::lifecycle::{platform_lifecycle, ExitEntry};
use music_player_rs::platform::notify::{platform_notifier, RtSlot};
use music_player_rs::platform::tray::{TrayChannels, TrayPort};
use music_player_rs::theme::create_default_themes;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// UI update period: pulls tray commands, scan/cover results and playback
/// state into the window. Also bounds responsiveness of transport controls.
const TICK_INTERVAL_MS: u64 = 100;

/// High-frequency column reflow period. Reads the playlist width and updates
/// column `.width`s nearly at display rate so the table tracks the window
/// during a resize instead of lagging behind on the slow 100 ms tick.
const REF_INTERVAL_MS: u64 = 16;

/// Live spectrum push period (~30 FPS; ТЗ §11.1). Independent of the 100 ms
/// tick — the bar data needs an animation-rate refresh.
const VIZ_PUSH_INTERVAL_MS: u64 = app::visualizer_manager::VIZ_PUSH_INTERVAL_MS;

/// Срок дозаписи журнала при выходе; полный путь выхода — С4 (ADR-7, ADR-21).
const JOURNAL_FLUSH_BUDGET: Duration = Duration::from_secs(1);

/// Срок ожидания ответов писателя на команды `BadCopy` до создания окна
/// (ADR-23 шаг 3, §8 С4). Окна ещё нет — бюджет небольшой и локальный, а не
/// общий `EXIT_BUDGET` пути выхода.
const BAD_COPY_WAIT_BUDGET: Duration = Duration::from_secs(2);

fn main() {
    // Каталог настроек пользователя ищется только здесь (ТЗ-49, ADR-19):
    // остальной код получает пути из `ConfigPaths`.
    let paths = ConfigPaths::in_dir(music_player_rs::settings::config_dir());
    // T1.0 §1/§8.2: гарантировать `themes/` + `dark.toml`/`light.toml`.
    // Существующие файлы не перезаписываются (воссоздаются только
    // отсутствующие, в т.ч. после ручного удаления).
    if let Err(e) = create_default_themes(&paths.dir.join("themes")) {
        eprintln!("[theme] create_default_themes failed: {e}");
    }
    // Журнал и модуль ФС собираются только здесь (ADR-19, ADR-21).
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::start(paths.journal.clone()));
    let (reader, writer_fs, _engine_fs) = os_fs(journal.clone());
    // Чтение и разбор обоих файлов до создания окна, по одному разу каждый
    // (ADR-23 шаг 2, §6.1, ТЗ-1, ТЗ-4). Запись не производится здесь.
    let boot = persist::boot(reader.as_ref(), &paths);
    for rec in persist::journal_records_for_boot(&boot) {
        journal.record(rec);
    }
    // Писатель `apap-persist` (ADR-23 шаг 3, §6.6, ТЗ-3) — единственный
    // владелец записи рабочих файлов; UI-поток файлов не пишет (ТЗ-22).
    let writer = spawn_writer(writer_fs, paths.clone());
    // Копия `*.bad` неразбираемого файла пишется до первой записи этим же
    // файлом (И-Р18, ТЗ-6): команды `BadCopy` уходят писателю раньше любого
    // `Write` — FIFO писателя (§6.6) гарантирует нужный порядок без
    // синхронной записи (§8 С4, ADR-23 шаг 3).
    bad_copies_via_writer(&boot, &writer, journal.as_ref());
    // Движок запускается до AppCore — `deps` строит только main (ADR-19, ADR-01, ADR-20).
    let (engine_sink, engine_events) = app::engine_sink::event_channel(app::engine_sink::slint_wake());
    let engine = EngineHandle::spawn(EngineDeps::system(Box::new(engine_sink), paths.dir.clone(), journal.clone()));
    // `AppCore` — владелец действующих настроек и состояния, писателя и
    // инжектируемых часов/ожидания (ADR-19, ADR-23, §2.12, §6.1). `MusicApp`
    // владеет этим экземпляром; старое плоское поле настроек заполняется из
    // него через мост до шага очистки (§8.1 С3).
    let core = AppCore::with_deps(
        AppDeps {
            writer,
            paths: paths.clone(),
            clock: Box::new(MonotonicClock::new()),
            waiter: Box::new(ChannelWaiter),
            journal: journal.clone(),
            engine: match &engine {
                Ok(h) => Box::new(h.sender()),
                Err(_) => Box::new(NoEngine),
            },
        },
        boot,
    );
    let ui = match app::create_ui() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("не удалось создать окно Slint: {e}");
            journal.flush(JOURNAL_FLUSH_BUDGET);
            return;
        }
    };
    // Каналы трея (ADR-23 шаг 8): UI-концы — приложению, концы трея — в
    // `Lifecycle::install` после показа окна.
    let (tray_tx, tray_events) = std::sync::mpsc::channel();
    let (tray_updates, tray_updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let (tray_ready_tx, tray_ready) = std::sync::mpsc::channel();
    // Рантайм трея для системных уведомлений (ADR-9): заполняет поток трея.
    let notify_rt: RtSlot = Arc::default();
    let tray_port = TrayPort { updates: tray_updates_rx, ready: tray_ready_tx, rt: notify_rt.clone() };
    let notifier = platform_notifier(notify_rt, journal.clone());
    let tray = TrayChannels { events: tray_events, updates: tray_updates, ready: tray_ready };
    let app = Rc::new(RefCell::new(MusicApp::new(
        &ui,
        core,
        paths,
        tray,
        notifier,
        engine,
        engine_events,
    )));
    MusicApp::init(&app);

    // Пробуждение UI событием движка: слив очереди сразу, не дожидаясь тика
    // 100 мс. `Weak` — хук не продлевает жизнь `MusicApp`; `try_borrow_mut`
    // занят (вызов изнутри обработчика) — события догонит ближайший тик
    // (ADR-02, ТЗ-60, ТЗ-106).
    let app_for_drain = Rc::downgrade(&app);
    app::engine_sink::install_drain_hook(Box::new(move || {
        let Some(app) = app_for_drain.upgrade() else { return };
        let Ok(mut app) = app.try_borrow_mut() else { return };
        app.drain_engine_events();
    }));

    let weak = ui.as_weak();
    let app_for_tick = app.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(TICK_INTERVAL_MS),
        move || {
            if weak.upgrade().is_none() {
                return;
            }
            app_for_tick.borrow_mut().tick();
        },
    );

    let weak = ui.as_weak();
    let app_for_ref = app.clone();
    let ref_timer = slint::Timer::default();
    ref_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(REF_INTERVAL_MS),
        move || {
            if weak.upgrade().is_none() {
                return;
            }
            app_for_ref.borrow_mut().reflow();
        },
    );

    let weak = ui.as_weak();
    let app_for_viz = app.clone();
    let viz_timer = slint::Timer::default();
    viz_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(VIZ_PUSH_INTERVAL_MS),
        move || {
            if weak.upgrade().is_none() {
                return;
            }
            app_for_viz.borrow_mut().viz_push();
        },
    );

    if let Err(e) = ui.show() {
        eprintln!("не удалось показать окно: {e}");
        journal.flush(JOURNAL_FLUSH_BUDGET);
        return;
    }
    // Сообщения, поставленные в очередь до первого показа окна, выходят на
    // экран только теперь (И-Р9, ТЗ-52 п. 1).
    app.borrow_mut().window_shown();
    // Размер/позиция окна применимы только после создания поверхности:
    // winit игнорирует `set_size`/`set_position` до `show()`, и окно могло бы
    // схлопнуться до минимального размера контента. Повторно применяем
    // сохранённую геометрию (V5.1-B7) из видимого состояния.
    app.borrow().apply_window_geometry();
    // Шаг 8 порядка запуска (ADR-23): перехваты ОС ставятся после показа
    // окна, все пути выхода сходятся в одну точку `exit` (ADR-7, ТЗ-14).
    let mut lifecycle =
        platform_lifecycle(Some((tray_port, Box::new(MonotonicClock::new()))));
    if let Err(e) = lifecycle.install(Some(ui.window()), exit_entry(&app), tray_tx) {
        eprintln!("перехваты ОС не установлены: {}: {}", e.what, e.detail);
    }
    if let Err(e) = slint::run_event_loop_until_quit() {
        eprintln!("цикл событий завершился с ошибкой: {e}");
    }
    drop(viz_timer);
    drop(ref_timer);
    drop(timer);
    // Дописать строки журнала до выхода процесса (ADR-21).
    journal.flush(JOURNAL_FLUSH_BUDGET);
}

/// Единая точка выхода для обработчиков ОС (ADR-7, ТЗ-14). Слабая ссылка —
/// без цикла `Rc`; приложение уже заимствовано (вложенный цикл сообщений ОС)
/// → `Busy`, без записи (ADR-7 п. 4).
fn exit_entry(app: &Rc<RefCell<MusicApp>>) -> ExitEntry {
    let weak = Rc::downgrade(app);
    Rc::new(move |reason: ExitReason| {
        let Some(app) = weak.upgrade() else {
            return ExitOutcome::Ignored;
        };
        let Ok(mut app) = app.try_borrow_mut() else {
            return ExitOutcome::Busy;
        };
        app.exit(reason)
    })
}

/// Копии `*.bad` неразбираемых файлов через писателя `apap-persist` (ТЗ-6,
/// ТЗ-22, И-Р18, ADR-23 шаг 3, §8 С4). Команды `BadCopy` отправляются до
/// создания `AppCore` — на этот момент писателю ещё не послано ни одного
/// `Write`, поэтому FIFO очереди (§6.6) сам гарантирует нужный порядок:
/// копия пишется раньше первой записи этим же файлом.
///
/// Ждёт ровно столько ответов, сколько команд отправлено, в пределах
/// `BAD_COPY_WAIT_BUDGET` суммарно (окна ещё нет — таймаут небольшой и
/// локальный). Любой ответ, пришедший раньше срока, конвертируется в
/// `persist::BadCopyOutcome` и попадает в журнал через
/// `persist::journal_records_for_bad_copies`; файл, на который ответ не
/// успел прийти, просто не получает записи журнала (функция строит записи
/// по числу элементов `outcomes`, а не по числу неразбираемых файлов).
/// Ответ другого типа здесь не ожидается — `Write` до этого момента не
/// отправлялся — и игнорируется.
fn bad_copies_via_writer(boot: &Boot, writer: &WriterHandle, journal: &dyn Journal) {
    let mut expected: u32 = 0;
    if let Parsed::Unparsable { original, .. } = &boot.settings {
        writer.send(WriterCmd::BadCopy { file: ConfigFile::Settings, bytes: original.clone() });
        expected += 1;
    }
    if let Parsed::Unparsable { original, .. } = &boot.state {
        writer.send(WriterCmd::BadCopy { file: ConfigFile::State, bytes: original.clone() });
        expected += 1;
    }
    if expected == 0 {
        return;
    }

    let deadline = Instant::now() + BAD_COPY_WAIT_BUDGET;
    let mut outcomes: Vec<BadCopyOutcome> = Vec::new();
    while outcomes.len() < expected as usize {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match writer.replies().recv_timeout(remaining) {
            Ok(WriterReply::BadCopySaved { file, path }) => outcomes.push(BadCopyOutcome { file, result: Ok(path) }),
            Ok(WriterReply::BadCopyFailed { file, err }) => outcomes.push(BadCopyOutcome { file, result: Err(err) }),
            Ok(_) => {}
            Err(_) => break,
        }
    }

    for rec in persist::journal_records_for_bad_copies(boot, &outcomes) {
        journal.record(rec);
    }
}
