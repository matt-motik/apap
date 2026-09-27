//! wheel_probe — эксперимент задачи `docs/02_settings_persistence_v1.0/`:
//! запись сырых дельт колёсика мыши и тачпада над ползунком Slint (как в
//! `ui/top_panel.slint` плеера) и над иконкой трея (ksni на Linux).
//!
//! Программа ничего не нормализует и не округляет: в CSV пишутся значения
//! в том виде, в котором их отдали Slint (`scroll-event`), winit
//! (`WindowEvent::MouseWheel`) и хост трея (`ksni::Tray::scroll`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slint::winit_030::winit::event::{MouseScrollDelta, TouchPhase, WindowEvent};
use slint::winit_030::winit::keyboard::ModifiersState;
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Timer, TimerMode};

slint::slint! {
    import { Button, ComboBox, Slider, VerticalBox, HorizontalBox } from "std-widgets.slint";

    export component ProbeWindow inherits Window {
        title: "wheel_probe — замер колёсика и тачпада";
        preferred-width: 720px;
        preferred-height: 640px;

        in property <string> progress;
        in property <string> step-title;
        in property <string> instruction;
        in property <string> counters;
        in property <string> status;
        in property <bool> finished;
        in-out property <int> device-index;
        in-out property <int> natural-index;
        in-out property <float> volume: 0.5;

        callback scrolled(float, float, bool, bool, bool, bool);
        callback done();
        callback repeat();
        callback skip();
        callback skip-device();

        VerticalBox {
            Text { text: root.progress; }
            Text { text: root.step-title; font-size: 18px; font-weight: 700; wrap: word-wrap; }
            Text { text: root.instruction; wrap: word-wrap; }

            HorizontalBox {
                Text { text: "Устройство:"; vertical-alignment: center; }
                ComboBox {
                    model: ["мышь с колесом", "тачпад (двумя пальцами)", "мышь с высокоточным / свободным колесом"];
                    current-index <=> root.device-index;
                    enabled: !root.finished;
                }
            }
            HorizontalBox {
                Text { text: "Естественная прокрутка в системе:"; vertical-alignment: center; }
                ComboBox {
                    model: ["не знаю", "включена", "выключена"];
                    current-index <=> root.natural-index;
                    enabled: !root.finished;
                }
            }

            // Копия ползунка громкости плеера (ui/top_panel.slint:464-492):
            // TouchArea со scroll-event вокруг вертикального Slider.
            HorizontalBox {
                alignment: center;
                height: 240px;
                wheel-vol := TouchArea {
                    vertical-stretch: 1;
                    width: 34px;
                    scroll-event(event) => {
                        root.scrolled(event.delta-x / 1px, event.delta-y / 1px,
                            event.modifiers.shift, event.modifiers.control,
                            event.modifiers.alt, event.modifiers.meta);
                        if (event.delta-y != 0) {
                            root.volume = Math.clamp(root.volume + (event.delta-y > 0 ? 0.04 : -0.04), 0, 1);
                            return accept;
                        }
                        reject
                    }
                    HorizontalLayout {
                        alignment: center;
                        vertical-stretch: 1;
                        Slider {
                            vertical-stretch: 1;
                            width: 24px;
                            value: root.volume;
                            minimum: 0;
                            maximum: 1;
                            orientation: vertical;
                            changed(v) => { root.volume = v; }
                        }
                    }
                }
            }

            Text { text: root.counters; wrap: word-wrap; }
            Text { text: root.status; wrap: word-wrap; color: #b05000; }

            HorizontalBox {
                Button { text: "Готово"; enabled: !root.finished; clicked => { root.done(); } }
                Button { text: "Повторить"; enabled: !root.finished; clicked => { root.repeat(); } }
                Button { text: "Пропустить"; enabled: !root.finished; clicked => { root.skip(); } }
                Button { text: "Пропустить устройство"; enabled: !root.finished; clicked => { root.skip-device(); } }
            }
        }
    }
}

// ---------------------------------------------------------------- шаги

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Device {
    Mouse,
    Touchpad,
    HiRes,
}

impl Device {
    fn slug(self) -> &'static str {
        match self {
            Device::Mouse => "mouse",
            Device::Touchpad => "touchpad",
            Device::HiRes => "hires",
        }
    }
    fn index(self) -> i32 {
        match self {
            Device::Mouse => 0,
            Device::Touchpad => 1,
            Device::HiRes => 2,
        }
    }
    fn from_index(i: i32) -> Device {
        match i {
            1 => Device::Touchpad,
            2 => Device::HiRes,
            _ => Device::Mouse,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Device::Mouse => "мышь с колесом",
            Device::Touchpad => "тачпад",
            Device::HiRes => "мышь с высокоточным / свободным колесом",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Slider,
    Tray,
}

impl Target {
    fn slug(self) -> &'static str {
        match self {
            Target::Slider => "slider",
            Target::Tray => "tray",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dir {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug)]
struct Action {
    slug: &'static str,
    text: &'static str,
    dir: Dir,
}

const MOUSE_ACTIONS: &[Action] = &[
    Action { slug: "single_up", text: "10 одиночных щелчков колёсиком ВВЕРХ с паузой ≈1 с", dir: Dir::Up },
    Action { slug: "single_down", text: "10 одиночных щелчков колёсиком ВНИЗ с паузой ≈1 с", dir: Dir::Down },
    Action { slug: "fast_up", text: "быстрая прокрутка ВВЕРХ ≈10 щелчков одним движением", dir: Dir::Up },
    Action { slug: "shift_single_up", text: "с зажатым Shift: 10 одиночных щелчков ВВЕРХ с паузой ≈1 с", dir: Dir::Up },
];

const TOUCHPAD_ACTIONS: &[Action] = &[
    Action { slug: "short_up", text: "короткий свайп двумя пальцами ВВЕРХ (≈1 см), 5 раз с паузой ≈1 с", dir: Dir::Up },
    Action { slug: "long_slow_up", text: "длинный медленный свайп двумя пальцами ВВЕРХ (через весь тачпад, ≈2 с), 3 раза", dir: Dir::Up },
    Action { slug: "fast_up", text: "быстрый свайп двумя пальцами ВВЕРХ (с «броском»), 3 раза с паузой ≈2 с", dir: Dir::Up },
    Action { slug: "short_down", text: "короткий свайп двумя пальцами ВНИЗ (≈1 см), 5 раз с паузой ≈1 с", dir: Dir::Down },
    Action { slug: "long_slow_down", text: "длинный медленный свайп двумя пальцами ВНИЗ (через весь тачпад, ≈2 с), 3 раза", dir: Dir::Down },
    Action { slug: "fast_down", text: "быстрый свайп двумя пальцами ВНИЗ (с «броском»), 3 раза с паузой ≈2 с", dir: Dir::Down },
];

#[derive(Clone, Debug)]
struct Step {
    device: Device,
    target: Target,
    action: Action,
    /// Для тачпада шаги повторяются при включённой и выключенной естественной прокрутке.
    natural: Option<bool>,
}

fn build_steps(tray_supported: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    let targets: &[Target] = if tray_supported { &[Target::Slider, Target::Tray] } else { &[Target::Slider] };
    let groups: [(Device, Option<bool>, &[Action]); 4] = [
        (Device::Mouse, None, MOUSE_ACTIONS),
        (Device::Touchpad, Some(true), TOUCHPAD_ACTIONS),
        (Device::Touchpad, Some(false), TOUCHPAD_ACTIONS),
        (Device::HiRes, None, MOUSE_ACTIONS),
    ];
    for (device, natural, actions) in groups {
        for &target in targets {
            for &action in actions {
                steps.push(Step { device, target, action, natural });
            }
        }
    }
    steps
}

// ---------------------------------------------------------------- события

#[derive(Clone, Debug)]
struct Event {
    at: Instant,
    source: &'static str,
    delta_x: String,
    delta_y: String,
    orientation: String,
    modifiers: String,
}

/// Событие трея из потока ksni: момент получения, delta, orientation.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct TrayEvent {
    at: Instant,
    delta: i32,
    orientation: String,
}

fn mods_text(shift: bool, ctrl: bool, alt: bool, meta: bool) -> String {
    let mut parts = Vec::new();
    if shift {
        parts.push("shift");
    }
    if ctrl {
        parts.push("ctrl");
    }
    if alt {
        parts.push("alt");
    }
    if meta {
        parts.push("meta");
    }
    parts.join("+")
}

// ---------------------------------------------------------------- трей

#[cfg(target_os = "linux")]
mod tray {
    use super::TrayEvent;
    use std::sync::mpsc;
    use std::time::Instant;

    struct ProbeTray {
        tx: mpsc::Sender<TrayEvent>,
    }

    impl ksni::Tray for ProbeTray {
        fn id(&self) -> String {
            "wheel-probe".into()
        }
        fn icon_name(&self) -> String {
            "input-mouse".into()
        }
        fn title(&self) -> String {
            "wheel_probe".into()
        }
        fn scroll(&mut self, delta: i32, orientation: ksni::Orientation) {
            // Момент фиксируется здесь, в потоке трея, до передачи в UI.
            let _ = self.tx.send(TrayEvent { at: Instant::now(), delta, orientation: format!("{orientation:?}") });
        }
    }

    /// Запускает трей так же, как плеер (`src/tray.rs`): отдельный поток,
    /// tokio current_thread, `Tray::spawn`. Возвращает канал событий и канал
    /// итога запуска (Ok или текст ошибки).
    pub fn start() -> (mpsc::Receiver<TrayEvent>, mpsc::Receiver<Result<(), String>>) {
        use ksni::TrayMethods;
        let (tx, rx) = mpsc::channel();
        let (st_tx, st_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = st_tx.send(Err(format!("tokio: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                match (ProbeTray { tx }).spawn().await {
                    Ok(_handle) => {
                        let _ = st_tx.send(Ok(()));
                        // Держим runtime живым, пока жив процесс.
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        let _ = st_tx.send(Err(format!("{e}")));
                    }
                }
            });
        });
        (rx, st_rx)
    }
}

// ---------------------------------------------------------------- окружение

fn run_cmd(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn os_version() -> String {
    match std::env::consts::OS {
        "linux" => {
            let pretty = fs::read_to_string("/etc/os-release")
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
                })
                .unwrap_or_else(|| "?".into());
            let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").map(|k| k.trim().to_string()).unwrap_or_else(|_| "?".into());
            format!("{pretty}; ядро {kernel}")
        }
        "windows" => run_cmd("cmd", &["/C", "ver"]).unwrap_or_else(|| "Windows ?".into()),
        "macos" => format!("macOS {}", run_cmd("sw_vers", &["-productVersion"]).unwrap_or_else(|| "?".into())),
        other => other.to_string(),
    }
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn desktop_slug() -> String {
    match std::env::consts::OS {
        "windows" => "windows".into(),
        "macos" => "macos".into(),
        _ => {
            let d = env_var("XDG_CURRENT_DESKTOP");
            let first = d.split(':').next().unwrap_or("").to_lowercase();
            if first.is_empty() { "unknown".into() } else { first }
        }
    }
}

/// Дата и время UTC «ГГГГ-ММ-ДД ЧЧ:ММ:СС» без внешних крейтов.
fn utc_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Алгоритм civil_from_days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ---------------------------------------------------------------- запись

struct StepResult {
    file: String,
    step: Step,
    device_chosen: Device,
    natural_chosen: &'static str,
    duration_ms: f64,
    events: Vec<Event>,
}

struct Probe {
    steps: Vec<Step>,
    current: usize,
    step_start: Instant,
    events: Vec<Event>,
    results: Vec<String>,
    skipped: Vec<String>,
    out_dir: Option<PathBuf>,
    backend: String,
    tray_note: String,
    winit_mods: ModifiersState,
}

fn natural_label(i: i32) -> &'static str {
    match i {
        1 => "включена",
        2 => "выключена",
        _ => "не знаю",
    }
}

fn fmt_f(v: f64) -> String {
    // Кратчайшее представление без округления (как Display у f64).
    format!("{v}")
}

impl Probe {
    fn dir(&mut self) -> PathBuf {
        if let Some(d) = &self.out_dir {
            return d.clone();
        }
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
        let name = format!("{}_{}_{}", std::env::consts::OS, self.backend, desktop_slug());
        let mut dir = base.join(&name);
        let mut n = 2;
        while dir.exists() {
            dir = base.join(format!("{name}-{n}"));
            n += 1;
        }
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("wheel_probe: не создать {}: {e}", dir.display());
        }
        self.out_dir = Some(dir.clone());
        dir
    }

    fn write_env(&mut self) {
        let dir = self.dir();
        let mut t = String::new();
        let _ = writeln!(t, "date: {}", utc_now());
        let _ = writeln!(t, "os: {} ({})", std::env::consts::OS, os_version());
        let _ = writeln!(t, "arch: {}", std::env::consts::ARCH);
        let _ = writeln!(t, "window_backend (raw window handle): {}", self.backend);
        for v in ["XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE", "DESKTOP_SESSION", "KDE_SESSION_VERSION", "WAYLAND_DISPLAY", "DISPLAY"] {
            let _ = writeln!(t, "{v}: {}", env_var(v));
        }
        if let Some(v) = run_cmd("plasmashell", &["--version"]) {
            let _ = writeln!(t, "plasmashell: {v}");
        }
        if let Some(v) = run_cmd("gnome-shell", &["--version"]) {
            let _ = writeln!(t, "gnome-shell: {v}");
        }
        let _ = writeln!(t, "crates: slint 1.17.1 (renderer-software, style fluent), winit 0.30.13, ksni 0.3.6 (linux), tokio 1.53.1 (linux)");
        let _ = writeln!(t, "slint LineDelta → px: ×60 (i-slint-backend-winit 1.17.1, event_loop.rs:403); PixelDelta → логические px");
        let _ = writeln!(t, "tray: {}", self.tray_note);
        let _ = writeln!(t, "csv columns: t_ms,source,delta_x,delta_y,orientation,modifiers");
        let _ = writeln!(t, "  source=slint: scroll-event TouchArea ползунка; delta в логических px; orientation пусто");
        let _ = writeln!(t, "  source=winit: WindowEvent::MouseWheel окна; orientation = line|pixel/<TouchPhase>; delta — сырые значения winit");
        let _ = writeln!(t, "  source=tray: ksni Tray::scroll; delta_y = delta (i32), delta_x пусто; orientation = Vertical|Horizontal");
        if let Err(e) = fs::write(dir.join("env.txt"), t) {
            eprintln!("wheel_probe: env.txt: {e}");
        }
    }

    fn write_step(&mut self, device_chosen: Device, natural_chosen: &'static str) {
        let Some(step) = self.steps.get(self.current).cloned() else { return };
        let dir = self.dir();
        let action = match step.natural {
            Some(true) => format!("{}-natural_on", step.action.slug),
            Some(false) => format!("{}-natural_off", step.action.slug),
            None => step.action.slug.to_string(),
        };
        let file = format!("{:02}_{}_{}_{}.csv", self.current + 1, device_chosen.slug(), step.target.slug(), action);
        let mut csv = String::from("t_ms,source,delta_x,delta_y,orientation,modifiers\n");
        for e in &self.events {
            let t_ms = e.at.saturating_duration_since(self.step_start).as_secs_f64() * 1000.0;
            let _ = writeln!(csv, "{},{},{},{},{},{}", fmt_f(t_ms), e.source, e.delta_x, e.delta_y, e.orientation, e.modifiers);
        }
        if let Err(e) = fs::write(dir.join(&file), csv) {
            eprintln!("wheel_probe: {file}: {e}");
        }
        let res = StepResult {
            file,
            step,
            device_chosen,
            natural_chosen,
            duration_ms: self.step_start.elapsed().as_secs_f64() * 1000.0,
            events: std::mem::take(&mut self.events),
        };
        self.results.push(summarize(&res, self.step_start));
        self.write_summary();
    }

    fn write_summary(&mut self) {
        let dir = self.dir();
        let mut t = String::new();
        let _ = writeln!(t, "wheel_probe summary — только сырые величины, без нормализации");
        let _ = writeln!(t, "tray: {}\n", self.tray_note);
        for r in &self.results {
            t.push_str(r);
            t.push('\n');
        }
        if !self.skipped.is_empty() {
            let _ = writeln!(t, "Пропущено:");
            for s in &self.skipped {
                let _ = writeln!(t, "  {s}");
            }
        }
        if let Err(e) = fs::write(dir.join("summary.txt"), t) {
            eprintln!("wheel_probe: summary.txt: {e}");
        }
    }
}

fn parse(v: &str) -> Option<f64> {
    v.parse::<f64>().ok()
}

fn summarize(r: &StepResult, start: Instant) -> String {
    let mut t = String::new();
    let expected = match r.step.action.dir {
        Dir::Up => "вверх",
        Dir::Down => "вниз",
    };
    let natural_step = match r.step.natural {
        Some(true) => "шаг: естественная прокрутка ВКЛ",
        Some(false) => "шаг: естественная прокрутка ВЫКЛ",
        None => "",
    };
    let _ = writeln!(
        t,
        "== {} | устройство: {} | цель: {} | действие: {} ({}) | естественная прокрутка (ответ): {} {} | длительность {} мс",
        r.file,
        r.device_chosen.label(),
        r.step.target.slug(),
        r.step.action.slug,
        expected,
        r.natural_chosen,
        natural_step,
        fmt_f(r.duration_ms)
    );
    let mut by_source: BTreeMap<&str, Vec<&Event>> = BTreeMap::new();
    for e in &r.events {
        by_source.entry(e.source).or_default().push(e);
    }
    if by_source.is_empty() {
        let _ = writeln!(t, "   событий нет");
    }
    for (source, evs) in by_source {
        let _ = writeln!(t, "   source={source} events={}", evs.len());
        for (axis, pick) in [("x", 0usize), ("y", 1usize)] {
            let vals: Vec<&str> = evs.iter().map(|e| if pick == 0 { e.delta_x.as_str() } else { e.delta_y.as_str() }).filter(|v| !v.is_empty()).collect();
            if vals.is_empty() {
                continue;
            }
            let mut uniq: BTreeMap<&str, usize> = BTreeMap::new();
            for v in &vals {
                *uniq.entry(v).or_default() += 1;
            }
            let nums: Vec<f64> = vals.iter().filter_map(|v| parse(v)).collect();
            let min = nums.iter().copied().fold(f64::INFINITY, f64::min);
            let max = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let sum: f64 = nums.iter().sum();
            let sign = if sum > 0.0 { "+" } else if sum < 0.0 { "-" } else { "0" };
            let list: Vec<String> = uniq.iter().take(24).map(|(v, n)| format!("{v}×{n}")).collect();
            let more = if uniq.len() > 24 { format!(" …(+{})", uniq.len() - 24) } else { String::new() };
            let _ = writeln!(
                t,
                "     delta_{axis}: уникальных {} [{}{}] min {} max {} сумма {} знак суммы {} (ожидалось «{}»)",
                uniq.len(),
                list.join(", "),
                more,
                fmt_f(min),
                fmt_f(max),
                fmt_f(sum),
                sign,
                expected
            );
        }
        let mut orients: BTreeMap<&str, usize> = BTreeMap::new();
        for e in &evs {
            if !e.orientation.is_empty() {
                *orients.entry(e.orientation.as_str()).or_default() += 1;
            }
        }
        if !orients.is_empty() {
            let list: Vec<String> = orients.iter().map(|(o, n)| format!("{o}×{n}")).collect();
            let _ = writeln!(t, "     orientation: {}", list.join(", "));
        }
        let mut mods: BTreeMap<&str, usize> = BTreeMap::new();
        for e in &evs {
            if !e.modifiers.is_empty() {
                *mods.entry(e.modifiers.as_str()).or_default() += 1;
            }
        }
        if !mods.is_empty() {
            let list: Vec<String> = mods.iter().map(|(o, n)| format!("{o}×{n}")).collect();
            let _ = writeln!(t, "     modifiers: {}", list.join(", "));
        }
        let times: Vec<f64> = evs.iter().map(|e| e.at.saturating_duration_since(start).as_secs_f64() * 1000.0).collect();
        let mut dts: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
        dts.sort_by(f64::total_cmp);
        if !dts.is_empty() {
            let med = if dts.len() % 2 == 1 { dts[dts.len() / 2] } else { (dts[dts.len() / 2 - 1] + dts[dts.len() / 2]) / 2.0 };
            let _ = writeln!(t, "     интервалы: медиана {} мс, min {} мс, max {} мс", fmt_f(med), fmt_f(dts[0]), fmt_f(dts[dts.len() - 1]));
        }
    }
    t
}

// ---------------------------------------------------------------- UI

fn refresh(ui: &ProbeWindow, p: &Probe) {
    let total = p.steps.len();
    if p.current >= total {
        ui.set_finished(true);
        ui.set_progress(format!("Готово: {total} шагов").into());
        ui.set_step_title("Замер завершён".into());
        let dir = p.out_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
        ui.set_instruction(format!("Результаты записаны в {dir}. Закройте окно.").into());
        ui.set_counters("".into());
        return;
    }
    let Some(step) = p.steps.get(p.current) else { return };
    ui.set_progress(format!("Шаг {} из {}", p.current + 1, total).into());
    let target = match step.target {
        Target::Slider => "наведите указатель на ПОЛЗУНОК в этом окне",
        Target::Tray => "наведите указатель на ИКОНКУ «wheel_probe» в трее (область уведомлений)",
    };
    let natural = match step.natural {
        Some(true) => "Перед шагом ВКЛЮЧИТЕ естественную прокрутку тачпада в настройках системы.\n",
        Some(false) => "Перед шагом ВЫКЛЮЧИТЕ естественную прокрутку тачпада в настройках системы.\n",
        None => "",
    };
    ui.set_step_title(format!("{} → {}", step.device.label(), step.target.slug()).into());
    ui.set_instruction(
        format!(
            "{}Затем {}: {}.\nПроверьте поля «Устройство» и «Естественная прокрутка», потом нажмите «Готово». «Повторить» — начать шаг заново.",
            natural, target, step.action.text
        )
        .into(),
    );
    ui.set_device_index(step.device.index());
    if let Some(n) = step.natural {
        ui.set_natural_index(if n { 1 } else { 2 });
    }
    ui.set_counters("Событий: 0".into());
}

fn counters_text(p: &Probe) -> String {
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &p.events {
        *by.entry(e.source).or_default() += 1;
    }
    let list: Vec<String> = by.iter().map(|(s, n)| format!("{s}: {n}")).collect();
    if list.is_empty() { "Событий: 0".into() } else { format!("Событий: {}", list.join(", ")) }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    let (tray_rx, tray_status) = tray::start();
    // У плеера трей есть только на Linux (ksni / StatusNotifierItem, src/tray.rs).
    #[cfg(target_os = "linux")]
    let tray_supported = true;
    #[cfg(not(target_os = "linux"))]
    let tray_supported = false;

    let ui = ProbeWindow::new()?;
    let probe = Rc::new(RefCell::new(Probe {
        steps: build_steps(tray_supported),
        current: 0,
        step_start: Instant::now(),
        events: Vec::new(),
        results: Vec::new(),
        skipped: Vec::new(),
        out_dir: None,
        backend: "unknown".into(),
        tray_note: if tray_supported {
            "ksni: запуск…".into()
        } else {
            "у плеера нет трея на этой ОС (трей — только ksni/StatusNotifierItem, src/tray.rs); шаги трея пропущены".into()
        },
        winit_mods: ModifiersState::empty(),
    }));

    // Slint scroll-event ползунка.
    {
        let probe = probe.clone();
        let weak = ui.as_weak();
        ui.on_scrolled(move |dx, dy, shift, ctrl, alt, meta| {
            let mut p = probe.borrow_mut();
            p.events.push(Event {
                at: Instant::now(),
                source: "slint",
                delta_x: format!("{dx}"),
                delta_y: format!("{dy}"),
                orientation: String::new(),
                modifiers: mods_text(shift, ctrl, alt, meta),
            });
            if let Some(ui) = weak.upgrade() {
                ui.set_counters(counters_text(&p).into());
            }
        });
    }

    // Сырые события winit окна.
    {
        let probe = probe.clone();
        ui.window().on_winit_window_event(move |_w, ev| {
            match ev {
                WindowEvent::ModifiersChanged(m) => {
                    probe.borrow_mut().winit_mods = m.state();
                }
                WindowEvent::MouseWheel { delta, phase, .. } => {
                    let phase = match phase {
                        TouchPhase::Started => "Started",
                        TouchPhase::Moved => "Moved",
                        TouchPhase::Ended => "Ended",
                        TouchPhase::Cancelled => "Cancelled",
                    };
                    let (kind, dx, dy) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => ("line", format!("{x}"), format!("{y}")),
                        MouseScrollDelta::PixelDelta(p) => ("pixel", format!("{}", p.x), format!("{}", p.y)),
                    };
                    let mut p = probe.borrow_mut();
                    let m = p.winit_mods;
                    let modifiers = mods_text(m.shift_key(), m.control_key(), m.alt_key(), m.super_key());
                    p.events.push(Event {
                        at: Instant::now(),
                        source: "winit",
                        delta_x: dx,
                        delta_y: dy,
                        orientation: format!("{kind}/{phase}"),
                        modifiers,
                    });
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }

    let advance = {
        let weak = ui.as_weak();
        move |p: &mut Probe| {
            p.current += 1;
            p.step_start = Instant::now();
            p.events.clear();
            if let Some(ui) = weak.upgrade() {
                refresh(&ui, p);
            }
        }
    };

    {
        let probe = probe.clone();
        let weak = ui.as_weak();
        let advance = advance.clone();
        ui.on_done(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut p = probe.borrow_mut();
            let device = Device::from_index(ui.get_device_index());
            let natural = natural_label(ui.get_natural_index());
            p.write_step(device, natural);
            advance(&mut p);
        });
    }
    {
        let probe = probe.clone();
        let weak = ui.as_weak();
        ui.on_repeat(move || {
            let mut p = probe.borrow_mut();
            p.events.clear();
            p.step_start = Instant::now();
            if let Some(ui) = weak.upgrade() {
                ui.set_counters(counters_text(&p).into());
            }
        });
    }
    {
        let probe = probe.clone();
        let advance = advance.clone();
        ui.on_skip(move || {
            let mut p = probe.borrow_mut();
            if let Some(s) = p.steps.get(p.current) {
                let line = format!("{:02} {} {} {} — пропущен пользователем", p.current + 1, s.device.slug(), s.target.slug(), s.action.slug);
                p.skipped.push(line);
            }
            p.write_summary();
            advance(&mut p);
        });
    }
    {
        let probe = probe.clone();
        let advance = advance.clone();
        ui.on_skip_device(move || {
            let mut p = probe.borrow_mut();
            let Some(cur) = p.steps.get(p.current).cloned() else { return };
            let line = format!("{:02}… {} (естественная прокрутка {:?}) — устройство пропущено пользователем", p.current + 1, cur.device.slug(), cur.natural);
            p.skipped.push(line);
            // Дойти до первого шага другого устройства (или другой группы тачпада).
            while let Some(s) = p.steps.get(p.current + 1) {
                if s.device == cur.device && s.natural == cur.natural {
                    p.current += 1;
                } else {
                    break;
                }
            }
            p.write_summary();
            advance(&mut p);
        });
    }

    refresh(&ui, &probe.borrow());

    // После показа окна: определить фактический бэкенд окна и записать env.txt.
    let env_timer = Timer::default();
    {
        let probe = probe.clone();
        let weak = ui.as_weak();
        env_timer.start(TimerMode::SingleShot, Duration::from_millis(300), move || {
            let Some(ui) = weak.upgrade() else { return };
            let backend = ui
                .window()
                .with_winit_window(|w| match w.window_handle().map(|h| h.as_raw()) {
                    Ok(RawWindowHandle::Wayland(_)) => "wayland",
                    Ok(RawWindowHandle::Xlib(_)) | Ok(RawWindowHandle::Xcb(_)) => "x11",
                    Ok(RawWindowHandle::Win32(_)) => "win32",
                    Ok(RawWindowHandle::AppKit(_)) => "appkit",
                    _ => "other",
                })
                .unwrap_or("not-winit");
            let mut p = probe.borrow_mut();
            p.backend = backend.to_string();
            p.step_start = Instant::now();
            p.write_env();
        });
    }

    // Опрос событий трея (из потока ksni) и статуса его запуска.
    let tray_timer = Timer::default();
    #[cfg(target_os = "linux")]
    {
        let probe = probe.clone();
        let weak = ui.as_weak();
        tray_timer.start(TimerMode::Repeated, Duration::from_millis(20), move || {
            let mut p = probe.borrow_mut();
            let mut changed = false;
            while let Ok(te) = tray_rx.try_recv() {
                p.events.push(Event {
                    at: te.at,
                    source: "tray",
                    delta_x: String::new(),
                    delta_y: format!("{}", te.delta),
                    orientation: te.orientation,
                    modifiers: String::new(),
                });
                changed = true;
            }
            if let Ok(st) = tray_status.try_recv() {
                p.tray_note = match st {
                    Ok(()) => "ksni: StatusNotifierItem зарегистрирован".into(),
                    Err(e) => format!("ksni: трей не запущен ({e}); шаги трея будут без событий — пропускайте их"),
                };
                if p.out_dir.is_some() {
                    p.write_env();
                }
                if let Some(ui) = weak.upgrade() {
                    ui.set_status(p.tray_note.clone().into());
                }
            }
            if changed {
                if let Some(ui) = weak.upgrade() {
                    ui.set_counters(counters_text(&p).into());
                }
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        ui.set_status(probe.borrow().tray_note.clone().into());
    }

    ui.run()?;
    drop(tray_timer);
    drop(env_timer);
    // Итог на случай закрытия окна посреди замера.
    let mut p = probe.borrow_mut();
    if p.out_dir.is_some() {
        p.write_summary();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_cover_table() {
        let with_tray = build_steps(true);
        // мышь 4×2 + тачпад 6×2×2 + высокоточное 4×2
        assert_eq!(with_tray.len(), 8 + 24 + 8);
        assert_eq!(build_steps(false).len(), 4 + 12 + 4);
    }

    #[test]
    fn utc_format_shape() {
        let s = utc_now();
        assert_eq!(s.len(), "2026-09-27 12:00:00 UTC".len());
    }
}
