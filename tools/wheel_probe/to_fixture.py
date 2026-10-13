#!/usr/bin/env python3
"""Перевод замеров wheel_probe из истории git в фикстуры колёсика.

Одноразовый скрипт этапа С11 (SP1.0, `docs/02_settings_persistence_v1.0/03_spec.md`
§7.4 «Фикстуры колёсика»). Читает CSV шага, `env.txt` и `summary.txt` замера
ТОЛЬКО командой `git show <коммит>:<путь>` — рабочее дерево
`tools/wheel_probe/results/` не читается и не меняется — и пишет
`tests/data/wheel/<замер>/<NN>_<шаг>.fixture`.

Формат фикстуры (§7.4):

    [fixture]
    platform = "kde"            # kde|gnome|windows|macos
    session = "wayland"         # wayland|x11|win32|appkit
    device = "mouse"            # mouse|touchpad (по имени шага и summary.txt)
    source = "slider"           # slider|tray — фактический, по событиям файла
    natural_scroll = "unknown"  # on|off|unknown
    direction = "up"            # up|down
    action = "single"           # single|fast|shift|short|long_slow
    expected_steps = [10]       # ожидаемые щелчки громкости, см. ниже
    origin = "<коммит>:<путь csv>"
    ---
    t_ms,kind,phase,delta_x,delta_y,modifiers,slint_dy

Строки данных:
  * событие winit  -> kind = line|pixel, phase = started|moved|ended|cancelled,
    delta_x/delta_y — сырые значения winit; slint_dy — delta_y парного
    scroll-event Slint (пусто, если пары нет);
  * событие tray   -> kind = tray, phase пусто; Vertical: delta ksni в delta_y,
    Horizontal: delta ksni в delta_x (§7.4 горизонталь не описывает);
  * scroll-event Slint без пары winit -> kind = slint, phase/delta_x/delta_y
    пусты, slint_dy заполнен (§7.4 этот случай не описывает).

Пара winit <-> slint: первое ещё не использованное событие slint между данным
событием winit и следующим событием winit, согласующееся по значению
(line: delta_y * 60 == slint delta_y; pixel: delta_y == slint delta_y;
допуск 1e-3).

Ожидаемые щелчки `expected_steps` (§7.4; `wheel_test.md` §5.1, §5.2):

  * единица — щелчок громкости; знак: `+` — громче, `−` — тише. Знак — как его
    даёт таблица знаков (`wheel_test.md` §5.3, В-5), системная инверсия уже
    применена ОС: у тачпада знак НЕ обязан совпадать с `direction`;
  * значимые события шага — те, что доходят до нормализатора (§6.11):
    source = slider — строки с непустым `slint_dy` (пара winit или kind = slint),
    значение в строках = slint_dy / 60; события winit без пары не считаются (В-2);
    source = tray — строки kind = tray с непустым `delta_y`, значение в делениях =
    sign * delta_y / division (KDE: +1, 120; GNOME: −1, 1 — В-5, §6.11);
  * свайп — серия значимых событий, в которой соседние разделены паузой
    не больше 300 мс; пауза строго больше 300 мс (t[i] − t[i−1] > 300, `t_ms`
    значимых строк) начинает новый свайп (§6.11 `swipe_gap_ms`, `wheel_test.md` §3);
  * ОДНО число — сумма щелчков за весь шаг: device = mouse. Равна числу событий
    со знаком `direction` (up → +, down → −); каждое событие — ровно одно деление;
  * НЕСКОЛЬКО чисел — щелчки по свайпам, в порядке свайпов: device = touchpad.
    Число элементов равно числу свайпов по правилу паузы; сумма списка — щелчки
    за весь шаг. Windows — столбец «T=0,5, K=5» таблицы §5.2 (явная таблица
    `WINDOWS_TOUCHPAD`); KDE-3 и GNOME-2 — сумма делений свайпа (эмулированное
    колесо: одно деление = один щелчок, без ограничения, В-3);
  * пустой список — ожидание не определено (сейчас таких шагов нет).

Скрипт ничего не подгоняет: вычисленное сверяется с числами протокола, и любое
расхождение завершает его ошибкой — число событий мыши (§5.2, §2.2:
`MOUSE_EVENTS`), число свайпов (§3.2–§3.4: `SWIPES`), диапазоны делений на
свайп (§5.2: `EMULATED_RANGE`, `EMULATED_OUTLIERS`), таблица тачпада Windows —
прогоном модели §5.1 / §6.11 (T = 0,5 строки, K = 5, пауза 300 мс) по данным шага.

Числа и модификаторы переносятся из исходника как есть (строками), порядок
строк — порядок исходника; повторный запуск даёт байт-в-байт тот же результат.
Всё, что нельзя вывести надёжно (неизвестное окружение, расхождение имени шага
с summary.txt, неизвестный вид события), завершает скрипт ошибкой.

Запуск:
    python tools/wheel_probe/to_fixture.py [--measure NAME ...] [--step NN ...]
                                           [--out DIR] [--check]
"""

import argparse
import csv
import io
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
RESULTS_DIR = "tools/wheel_probe/results"
DEFAULT_OUT = "tests/data/wheel"

# Замеры §7.4: имя -> (коммит, каталог в коммите).
MEASURES = {
    "kde-1": ("815d874", "linux_wayland_kde"),
    "kde-2": ("028f9f5", "linux_wayland_kde-2"),
    "kde-3": ("64edab4", "linux_wayland_kde-3"),
    "gnome-1": ("d7014de", "linux_wayland_ubuntu"),
    "gnome-2": ("d15caeb", "linux_not-winit_ubuntu"),
    "windows": ("eb6fb72", "windows_win32_windows"),
}

SRC_HEADER = ["t_ms", "source", "delta_x", "delta_y", "orientation", "modifiers"]
FIX_HEADER = ["t_ms", "kind", "phase", "delta_x", "delta_y", "modifiers", "slint_dy"]

# Имя шага: NN_<устройство>_<цель>_<действие>_<направление>[-natural_on|off].csv
STEP_RE = re.compile(
    r"^(?P<nn>\d{2})_(?P<device>mouse|touchpad)_(?P<target>slider|tray)_"
    r"(?P<action>[a-z_]+)_(?P<direction>up|down)"
    r"(?:-natural_(?P<natural>on|off))?\.csv$"
)
# Действие в имени шага -> action фикстуры, по устройству.
ACTIONS = {
    "mouse": {"single": "single", "fast": "fast", "shift_single": "shift"},
    "touchpad": {"short": "short", "long_slow": "long_slow", "fast": "fast"},
}
SUMMARY_DEVICE = {"мышь с колесом": "mouse", "тачпад": "touchpad"}
SUMMARY_DIRECTION = {"вверх": "up", "вниз": "down"}
SUMMARY_NATURAL = {"включена": "on", "выключена": "off", "не знаю": "unknown"}
WINIT_KINDS = ("line", "pixel")
WINIT_PHASES = ("started", "moved", "ended", "cancelled")
SESSIONS = ("wayland", "x11", "win32", "appkit")
LINE_PX = 60.0  # slint: LineDelta -> px (env.txt, i-slint-backend-winit)
PAIR_EPS = 1e-3

# --- expected_steps (03_spec.md §7.4; wheel_test.md §5.1, §5.2) ---------------

SWIPE_GAP_MS = 300.0  # граница свайпа: пауза строго больше (§6.11, wheel_test.md §3)
INT_EPS = 1e-6
DIRECTION_SIGN = {"up": 1, "down": -1}
# Трей по хосту: platform -> (знак, деление) (В-5; §6.11 tray_row; wheel_test.md §5.3).
TRAY_ROW = {"kde": (1, 120.0), "gnome": (-1, 1.0)}

# Мышь, wheel_test.md §5.2 (и §2.2): «Результат равен числу событий: 10 во всех
# шагах, кроме…». (замер, шаг) -> число событий.
MOUSE_EVENTS_DEFAULT = 10
MOUSE_EVENTS = {
    ("gnome-1", "01"): 11,
    ("gnome-2", "04"): 12,
    ("windows", "03"): 12,
    ("windows", "04"): 11,
    ("kde-3", "07"): 8,
    ("gnome-2", "07"): 7,
}

# Тачпад Windows, wheel_test.md §5.2, таблица «шаги на свайп», столбец
# «T=0,5, K=5» (решение В-4; «= T=0,5» раскрыто по столбцу «T=0,5»). Шаг -> свайпы.
WINDOWS_TOUCHPAD = {
    "05": [2, 2, 2, 4, 2],
    "08": [-2, -1, -1, -2, -3],
    "11": [-4, -3, -3, -3, -3],
    "14": [1, 1, 1, 1, 2],
    "06": [5, 5, 5],
    "09": [-5, -5, -5],
    "12": [-5, -5, -5],
    "15": [5, 5, 5],
    "07": [5, 5, 5],
    "10": [-5, -5, -5],
    "13": [-5, -5, -5],
    "16": [5, 5, 5],
}
# Параметры модели для сверки таблицы с данными (§5.1; §6.11 touch_step; В-4).
TOUCH_THRESHOLD_LINES = 0.5
TOUCH_MAX_STEPS = 5
INTEGER_AFTER_FRACTION_MS = 200.0

# Число свайпов тачпада, столбец «Свайпов» wheel_test.md §3.2 (KDE-3), §3.3
# (GNOME-2), §3.4 (Windows): короткий — 5, длинный и быстрый — 3, кроме…
SWIPES_DEFAULT = {"short": 5, "long_slow": 3, "fast": 3}
SWIPES = {
    ("kde-3", "12"): 6,
    ("kde-3", "18"): 4,
    ("kde-3", "24"): 7,
    ("kde-3", "28"): 4,
    ("gnome-2", "22"): 4,
}

# «Тачпад» KDE и GNOME (эмулированное колесо), wheel_test.md §5.2: делений на
# свайп — «короткий 1…21, длинный 10…42, быстрый 42…263».
EMULATED_RANGE = {"short": (1, 21), "long_slow": (10, 42), "fast": (42, 263)}
# Свайпы вне диапазонов §5.2: (замер, шаг) -> {номер свайпа с 1: |делений|}.
#   kde-3/15 — выброс 58 назван в wheel_test.md §4.5;
#   kde-3/18 (свайп 3: 51) и kde-3/28 (свайп 2: 1) в тексте §5.2 НЕ оговорены:
#   числа взяты из данных замера, а не из протокола.
EMULATED_OUTLIERS = {
    ("kde-3", "15"): {5: 58},
    ("kde-3", "18"): {3: 51},
    ("kde-3", "28"): {2: 1},
}


class FixtureError(Exception):
    """Данные нельзя перевести надёжно."""


def git(*args):
    """Вывод `git <args>` (utf-8) из корня репозитория."""
    proc = subprocess.run(
        ["git", *args], cwd=REPO_ROOT, capture_output=True, check=False
    )
    if proc.returncode != 0:
        err = proc.stderr.decode("utf-8", "replace").strip()
        raise FixtureError(f"git {' '.join(args)}: {err}")
    return proc.stdout.decode("utf-8")


def git_show(commit, path):
    return git("show", f"{commit}:{path}")


def list_csv(commit, directory):
    """Имена CSV-шагов каталога замера в коммите, по возрастанию."""
    out = git("ls-tree", "--name-only", commit, f"{RESULTS_DIR}/{directory}/")
    names = [line.rsplit("/", 1)[-1] for line in out.splitlines()]
    return sorted(n for n in names if n.endswith(".csv"))


def parse_env(text, where):
    """platform и session из env.txt."""
    env = {}
    for line in text.splitlines():
        if line.startswith(" ") or ":" not in line:
            continue
        key, value = line.split(":", 1)
        env[key.strip()] = value.strip()
    os_name = env.get("os", "").split(" ", 1)[0]
    desktop = env.get("XDG_CURRENT_DESKTOP", "").upper().split(":")
    if os_name == "windows":
        platform = "windows"
    elif os_name == "macos":
        platform = "macos"
    elif os_name == "linux" and "KDE" in desktop:
        platform = "kde"
    elif os_name == "linux" and "GNOME" in desktop:
        platform = "gnome"
    else:
        raise FixtureError(
            f"{where}: не удалось определить platform (os={env.get('os')!r}, "
            f"XDG_CURRENT_DESKTOP={env.get('XDG_CURRENT_DESKTOP')!r})"
        )
    backend = env.get("window_backend (raw window handle)", "")
    if backend in SESSIONS:
        session = backend
    elif backend == "not-winit" and env.get("XDG_SESSION_TYPE") in ("wayland", "x11"):
        # Окно не дало raw window handle winit — сеанс берётся из XDG_SESSION_TYPE.
        session = env["XDG_SESSION_TYPE"]
    else:
        raise FixtureError(
            f"{where}: не удалось определить session (window_backend={backend!r}, "
            f"XDG_SESSION_TYPE={env.get('XDG_SESSION_TYPE')!r})"
        )
    return platform, session


def parse_summary(text, where):
    """summary.txt -> {имя csv: {device, target, action, direction, natural, events}}."""
    steps = {}
    current = None
    for line in text.splitlines():
        if line.startswith("== "):
            parts = [p.strip() for p in line[3:].split("|")]
            fields = {}
            for part in parts[1:]:
                if ":" in part:
                    key, value = part.split(":", 1)
                    fields[key.strip()] = value.strip()
            try:
                action_raw = fields["действие"]
                match = re.match(r"^([a-z_]+) \((.+)\)$", action_raw)
                if not match:
                    raise KeyError(action_raw)
                # «естественная прокрутка (ответ): включена шаг: … ВКЛ» — берём ответ.
                natural_raw = fields["естественная прокрутка (ответ)"]
                natural_raw = natural_raw.split("шаг", 1)[0].strip()
                current = {
                    "device": SUMMARY_DEVICE[fields["устройство"]],
                    "target": fields["цель"],
                    "action": match.group(1),
                    "direction": SUMMARY_DIRECTION[match.group(2)],
                    "natural": SUMMARY_NATURAL[natural_raw],
                    "events": {},
                }
            except KeyError as exc:
                raise FixtureError(
                    f"{where}: непонятный заголовок шага ({exc}): {line}"
                ) from None
            steps[parts[0]] = current
            continue
        match = re.match(r"^\s+source=(\w+) events=(\d+)\s*$", line)
        if match and current is not None:
            current["events"][match.group(1)] = int(match.group(2))
    return steps


def step_meta(name, summary, where):
    """device/direction/action/natural_scroll шага: имя файла, сверенное с summary.txt."""
    match = STEP_RE.match(name)
    if not match:
        raise FixtureError(f"{where}: имя шага не по шаблону NN_<устройство>_<цель>_<действие>_<up|down>")
    device = match.group("device")
    action = ACTIONS[device].get(match.group("action"))
    if action is None:
        raise FixtureError(
            f"{where}: неизвестное действие {match.group('action')!r} для {device}"
        )
    info = summary.get(name)
    if info is None:
        raise FixtureError(f"{where}: шага нет в summary.txt")
    name_action = f"{match.group('action')}_{match.group('direction')}"
    checks = (
        ("устройство", device, info["device"]),
        ("цель", match.group("target"), info["target"]),
        ("действие", name_action, info["action"]),
        ("направление", match.group("direction"), info["direction"]),
    )
    for label, by_name, by_summary in checks:
        if by_name != by_summary:
            raise FixtureError(
                f"{where}: {label} по имени {by_name!r} != summary.txt {by_summary!r}"
            )
    natural = match.group("natural")
    if natural is None:
        natural = info["natural"]  # ответ пользователя: on|off|unknown
    elif natural != info["natural"]:
        raise FixtureError(
            f"{where}: natural по имени {natural!r} != summary.txt {info['natural']!r}"
        )
    return {
        "nn": match.group("nn"),
        "device": device,
        "target": match.group("target"),
        "action": action,
        "direction": match.group("direction"),
        "natural": natural,
        "events": info["events"],
    }


def read_events(text, where):
    reader = csv.reader(io.StringIO(text))
    header = next(reader, None)
    if header != SRC_HEADER:
        raise FixtureError(f"{where}: неожиданный заголовок CSV {header!r}")
    events = []
    for lineno, row in enumerate(reader, start=2):
        if not row:
            continue
        if len(row) != len(SRC_HEADER):
            raise FixtureError(f"{where}:{lineno}: {len(row)} полей вместо {len(SRC_HEADER)}")
        event = dict(zip(SRC_HEADER, row))
        if event["source"] not in ("winit", "slint", "tray"):
            raise FixtureError(f"{where}:{lineno}: неизвестный source {event['source']!r}")
        event["lineno"] = lineno
        events.append(event)
    return events


def to_float(value, where):
    try:
        return float(value)
    except ValueError:
        raise FixtureError(f"{where}: не число {value!r}") from None


def build_rows(events, where):
    """Строки данных фикстуры и счётчики."""
    rows = []
    stats = {"winit": 0, "tray": 0, "slint": 0, "paired": 0, "slint_unpaired": 0,
             "tray_horizontal": 0}
    # Событие slint -> индекс строки winit, с которой оно в паре.
    open_winit = None  # (индекс строки, kind, delta_y) последнего события winit без пары
    for event in events:
        at = f"{where}:{event['lineno']}"
        source = event["source"]
        stats[source] += 1
        if source == "winit":
            kind, sep, phase = event["orientation"].partition("/")
            phase = phase.lower()
            if not sep or kind not in WINIT_KINDS or phase not in WINIT_PHASES:
                raise FixtureError(f"{at}: неизвестный orientation winit {event['orientation']!r}")
            to_float(event["delta_x"], at)
            open_winit = (len(rows), kind, to_float(event["delta_y"], at))
            rows.append([event["t_ms"], kind, phase, event["delta_x"], event["delta_y"],
                         event["modifiers"], ""])
        elif source == "tray":
            if event["delta_x"] != "":
                raise FixtureError(f"{at}: у события tray непустой delta_x")
            to_float(event["delta_y"], at)
            if event["orientation"] == "Vertical":
                dx, dy = "", event["delta_y"]
            elif event["orientation"] == "Horizontal":
                dx, dy = event["delta_y"], ""
                stats["tray_horizontal"] += 1
            else:
                raise FixtureError(f"{at}: неизвестный orientation tray {event['orientation']!r}")
            # Событие трея не разрывает пару winit/slint: это другой источник.
            rows.append([event["t_ms"], "tray", "", dx, dy, event["modifiers"], ""])
        else:  # slint
            slint_dy = to_float(event["delta_y"], at)
            paired = False
            if open_winit is not None:
                index, kind, winit_dy = open_winit
                expected = winit_dy * LINE_PX if kind == "line" else winit_dy
                if abs(expected - slint_dy) <= PAIR_EPS:
                    rows[index][6] = event["delta_y"]
                    open_winit = None
                    paired = True
            if paired:
                stats["paired"] += 1
            else:
                stats["slint_unpaired"] += 1
                rows.append([event["t_ms"], "slint", "", "", "", event["modifiers"],
                             event["delta_y"]])
    return rows, stats


def effective_events(rows, source, platform, where):
    """Значимые события шага: [(t_ms, значение)] — строки (slider) или деления (tray)."""
    kinds = {row[1] for row in rows}
    events = []
    if source == "tray":
        if kinds != {"tray"}:
            raise FixtureError(
                f"{where}: в шаге трея есть события {sorted(kinds - {'tray'})} — "
                "expected_steps не определён"
            )
        if platform not in TRAY_ROW:
            raise FixtureError(f"{where}: нет знака и деления трея для platform={platform!r}")
        sign, division = TRAY_ROW[platform]
        for row in rows:
            if row[4] != "":  # вертикаль; горизонталь (delta_x) не считается (§6.11)
                events.append((to_float(row[0], where), sign * to_float(row[4], where) / division))
    else:
        for row in rows:
            if row[6] != "":
                events.append((to_float(row[0], where), to_float(row[6], where) / LINE_PX))
    if not events:
        raise FixtureError(f"{where}: нет значимых событий — expected_steps не определён")
    return events


def split_swipes(events):
    """Свайпы: новый начинается после паузы строго больше SWIPE_GAP_MS."""
    swipes = []
    last = None
    for t, value in events:
        if last is None or t - last > SWIPE_GAP_MS:
            swipes.append([])
        swipes[-1].append((t, value))
        last = t
    return swipes


def whole(value, where):
    """Целое число делений или ошибка."""
    nearest = round(value)
    if abs(value - nearest) > INT_EPS:
        raise FixtureError(f"{where}: значение {value!r} — не целое число делений")
    return int(nearest)


def expected_mouse(measure, meta, events, where):
    """Мышь (§5.2): одно число — число событий со знаком направления."""
    sign = DIRECTION_SIGN[meta["direction"]]
    for t, value in events:
        if whole(value, where) != sign:
            raise FixtureError(
                f"{where}: событие мыши t={t} даёт {value!r} делений, ожидалось {sign:+d} "
                f"(direction={meta['direction']})"
            )
    declared = MOUSE_EVENTS.get((measure, meta["nn"]), MOUSE_EVENTS_DEFAULT)
    if len(events) != declared:
        raise FixtureError(
            f"{where}: событий мыши {len(events)}, wheel_test.md §5.2 — {declared}"
        )
    return [sign * len(events)]


def check_swipe_count(measure, meta, swipes, where):
    declared = SWIPES.get((measure, meta["nn"]), SWIPES_DEFAULT[meta["action"]])
    if len(swipes) != declared:
        raise FixtureError(
            f"{where}: свайпов по паузе > {SWIPE_GAP_MS:g} мс — {len(swipes)}, "
            f"wheel_test.md §3.2–§3.4 — {declared}"
        )


def touchpad_model(events):
    """Щелчки по свайпам по модели §5.1 / §6.11 (тачпад Windows: T, K, пауза)."""
    per_swipe = []
    acc = 0.0
    steps = 0
    last = None
    last_fraction = None
    for t, value in events:
        if abs(value - round(value)) > INT_EPS:
            last_fraction = t
        elif last_fraction is None or t - last_fraction > INTEGER_AFTER_FRACTION_MS:
            return None  # событие класса «колесо»: модель тачпада неприменима
        if last is None or t - last > SWIPE_GAP_MS:
            acc = 0.0
            steps = 0
            per_swipe.append(0)
        last = t
        acc += value / TOUCH_THRESHOLD_LINES
        k = int(acc)  # к нулю
        acc -= k
        room = TOUCH_MAX_STEPS - abs(steps)
        k = max(-room, min(room, k))
        steps += k
        per_swipe[-1] += k
    return per_swipe


def expected_windows_touchpad(measure, meta, events, where):
    """Тачпад Windows (§5.2, столбец «T=0,5, K=5»): явная таблица, сверенная с данными."""
    table = WINDOWS_TOUCHPAD.get(meta["nn"])
    if table is None:
        raise FixtureError(f"{where}: шага нет в таблице wheel_test.md §5.2 (тачпад Windows)")
    swipes = split_swipes(events)
    check_swipe_count(measure, meta, swipes, where)
    if len(swipes) != len(table):
        raise FixtureError(
            f"{where}: свайпов {len(swipes)}, в таблице §5.2 — {len(table)} значений"
        )
    model = touchpad_model(events)
    if model != table:
        raise FixtureError(
            f"{where}: модель §5.1 (T=0,5, K=5) по данным даёт {model}, таблица §5.2 — {table}"
        )
    return list(table)


def expected_emulated_wheel(measure, meta, events, where):
    """«Тачпад» KDE/GNOME (§5.2, В-3): деления по свайпам, одно деление = один щелчок."""
    swipes = split_swipes(events)
    check_swipe_count(measure, meta, swipes, where)
    low, high = EMULATED_RANGE[meta["action"]]
    outliers = EMULATED_OUTLIERS.get((measure, meta["nn"]), {})
    result = []
    for number, swipe in enumerate(swipes, start=1):
        total = sum(whole(value, where) for _, value in swipe)
        if number in outliers:
            if abs(total) != outliers[number]:
                raise FixtureError(
                    f"{where}: свайп {number} — {total} делений, в EMULATED_OUTLIERS — "
                    f"{outliers[number]}"
                )
        elif not low <= abs(total) <= high:
            raise FixtureError(
                f"{where}: свайп {number} — {total} делений, wheel_test.md §5.2 — {low}…{high}"
            )
        result.append(total)
    for number in outliers:
        if number > len(swipes):
            raise FixtureError(f"{where}: в EMULATED_OUTLIERS свайп {number}, свайпов {len(swipes)}")
    if not (all(v > 0 for v in result) or all(v < 0 for v in result)):
        raise FixtureError(f"{where}: свайпы шага разного знака: {result}")
    return result


def expected_steps(measure, platform, source, meta, rows, where):
    """`expected_steps` шага (семантика — в docstring модуля)."""
    events = effective_events(rows, source, platform, where)
    if meta["device"] == "mouse":
        return expected_mouse(measure, meta, events, where)
    if platform == "windows":
        return expected_windows_touchpad(measure, meta, events, where)
    if platform in ("kde", "gnome"):
        return expected_emulated_wheel(measure, meta, events, where)
    raise FixtureError(f"{where}: нет правила expected_steps для тачпада на {platform!r}")


def render(header, rows):
    out = io.StringIO(newline="")
    out.write("[fixture]\n")
    for key, value in header:
        out.write(f"{key} = {value}\n")
    out.write("---\n")
    writer = csv.writer(out, lineterminator="\n")
    writer.writerow(FIX_HEADER)
    writer.writerows(rows)
    return out.getvalue()


def toml_str(value):
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def convert_step(measure, commit, directory, name, platform, session, summary):
    path = f"{RESULTS_DIR}/{directory}/{name}"
    where = f"{measure}/{name}"
    meta = step_meta(name, summary, where)
    events = read_events(git_show(commit, path), where)
    rows, stats = build_rows(events, where)
    for source in ("winit", "tray", "slint"):
        declared = meta["events"].get(source, 0)
        if declared != stats[source]:
            raise FixtureError(
                f"{where}: событий {source} в CSV {stats[source]}, в summary.txt {declared}"
            )
    if stats["tray"]:
        source = "tray"
    elif stats["winit"] or stats["slint"]:
        source = "slider"
    else:
        raise FixtureError(f"{where}: в шаге нет событий — source определить нельзя")
    expected = expected_steps(measure, platform, source, meta, rows, where)
    header = [
        ("platform", toml_str(platform)),
        ("session", toml_str(session)),
        ("device", toml_str(meta["device"])),
        ("source", toml_str(source)),
        ("natural_scroll", toml_str(meta["natural"])),
        ("direction", toml_str(meta["direction"])),
        ("action", toml_str(meta["action"])),
        ("expected_steps", "[" + ", ".join(str(v) for v in expected) + "]"),
        ("origin", toml_str(f"{commit}:{path}")),
    ]
    notes = []
    if source != meta["target"]:
        notes.append(f"имя шага — {meta['target']}, фактический source — {source}")
    if stats["tray"] and (stats["winit"] or stats["slint"]):
        notes.append("в шаге смешаны события tray и winit/slint")
    if stats["tray_horizontal"]:
        notes.append(f"горизонтальных событий tray: {stats['tray_horizontal']} (delta в delta_x)")
    if stats["slint_unpaired"]:
        notes.append(f"scroll-event без пары winit: {stats['slint_unpaired']} (kind = slint)")
    unpaired_winit = stats["winit"] - stats["paired"]
    if unpaired_winit:
        notes.append(f"событий winit без пары slint: {unpaired_winit}")
    return {
        "relpath": f"{measure}/{name[:-len('.csv')]}.fixture",
        "text": render(header, rows),
        "rows": len(rows),
        "stats": stats,
        "notes": notes,
    }


def convert(measures, steps):
    results = []
    for measure in measures:
        commit, directory = MEASURES[measure]
        base = f"{RESULTS_DIR}/{directory}"
        platform, session = parse_env(git_show(commit, f"{base}/env.txt"), f"{measure}/env.txt")
        summary = parse_summary(git_show(commit, f"{base}/summary.txt"), f"{measure}/summary.txt")
        names = list_csv(commit, directory)
        if not names:
            raise FixtureError(f"{measure}: в {commit}:{base} нет CSV")
        for name in names:
            if steps is not None and name[:2] not in steps:
                continue
            results.append(
                convert_step(measure, commit, directory, name, platform, session, summary)
            )
    return results


def main(argv=None):
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(
        description="Перевод замеров wheel_probe из истории git в фикстуры колёсика "
                    "(SP1.0 03_spec.md §7.4).",
    )
    parser.add_argument("--measure", nargs="+", choices=list(MEASURES), metavar="NAME",
                        help="замеры (по умолчанию все): " + ", ".join(MEASURES))
    parser.add_argument("--step", nargs="+", metavar="NN",
                        help="номера шагов (по умолчанию все), например 01 15")
    parser.add_argument("--out", metavar="DIR", default=None,
                        help=f"каталог фикстур (по умолчанию {DEFAULT_OUT} от корня репозитория)")
    parser.add_argument("--check", action="store_true",
                        help="ничего не писать, только вывести сводку")
    args = parser.parse_args(argv)

    steps = None
    if args.step:
        steps = set()
        for raw in args.step:
            if not raw.isdigit() or not 0 < int(raw) < 100:
                parser.error(f"--step: ожидается номер шага 01..99, получено {raw!r}")
            steps.add(f"{int(raw):02d}")
    measures = [m for m in MEASURES if not args.measure or m in args.measure]
    out_dir = Path(args.out) if args.out else REPO_ROOT / DEFAULT_OUT
    shown = args.out if args.out else DEFAULT_OUT
    shown = shown.replace("\\", "/").rstrip("/")

    try:
        results = convert(measures, steps)
        if not results:
            raise FixtureError("под заданные --measure/--step не попал ни один шаг")
    except FixtureError as exc:
        print(f"ошибка: {exc}", file=sys.stderr)
        return 2

    if not args.check:
        for item in results:
            target = out_dir / item["relpath"]
            target.parent.mkdir(parents=True, exist_ok=True)
            with open(target, "w", encoding="utf-8", newline="\n") as handle:
                handle.write(item["text"])

    print("файл\twinit\ttray\tslint\tстрок_данных\tзаметки")
    for item in results:
        stats = item["stats"]
        print(f"{shown}/{item['relpath']}\t{stats['winit']}\t{stats['tray']}\t"
              f"{stats['slint']}\t{item['rows']}\t{'; '.join(item['notes'])}")
    verb = "проверено" if args.check else "записано"
    print(f"{verb} файлов: {len(results)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
