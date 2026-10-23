#!/usr/bin/env python3
"""_STATE_.yaml <-> _STATE_.md tooling.

_STATE_.yaml is the source of truth for the current micro-session
(see docs/00_workflow_v1.0/planning.md, execution.md). _STATE_.md is a generated, human/agent-readable
rendering of it — it must never be hand-edited; `render` regenerates it and
`check` fails if it has drifted from what `render` would produce.

Usage:
    python tools/state_tool.py render            # (re)generate _STATE_.md from _STATE_.yaml
    python tools/state_tool.py check              # validate schema + business rules,
                                                   # fail if _STATE_.md is stale
    python tools/state_tool.py check-whitelist    # fail if any changed file (git status)
                                                   # is outside _STATE_.yaml's whitelist
    python tools/state_tool.py step-done [--next "<next_action>"]
                                                   # successful end of current_step
                                                   # (docs/00_workflow_v1.0/execution.md §4):
                                                   # marks it done, advances current_step to
                                                   # the next steps[] entry (list order, not
                                                   # id+1), resets fail_counter/failed_attempts,
                                                   # renders _STATE_.md, prints the chat line.
                                                   # --next is required unless current_step is
                                                   # the last entry (then current_step stays
                                                   # put and next_action becomes "закрытие по
                                                   # closing.md" regardless of --next).
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

import yaml
from jsonschema import Draft7Validator

# Files that are part of the workflow bookkeeping itself, not "code" — they
# legitimately change on nearly every commit regardless of the task's
# whitelist (see docs/00_workflow_v1.0/execution.md §4: _STATE_.md is
# committed together with the code on every successful step;
# docs/00_workflow_v1.0/closing.md §1: ROADMAP.md is patched at closing).
WHITELIST_EXEMPT = {"_STATE_.md", "_STATE_.yaml", "ROADMAP.md"}
# Acceptance results are written and committed by tools/acceptance.py run,
# which may happen while an unrelated micro-session is in progress.
WHITELIST_EXEMPT_RE = re.compile(r"^docs/acceptance/[^/]+/results-[a-z]+\.yaml$")

ROOT = Path(__file__).resolve().parent.parent
STATE_YAML = ROOT / "_STATE_.yaml"
STATE_MD = ROOT / "_STATE_.md"
SCHEMA_PATH = ROOT / "tools" / "schema" / "state.schema.json"

GENERATED_HEADER = (
    "<!-- GENERATED FILE — do not edit by hand.\n"
    "     Source of truth: _STATE_.yaml — edit that, then run:\n"
    "     python tools/state_tool.py render -->\n"
)


class StateError(Exception):
    """A schema or business-rule violation in _STATE_.yaml."""


def load_yaml_text(text: str) -> dict:
    return yaml.safe_load(text) or {}


def load_state() -> dict:
    if not STATE_YAML.exists():
        raise StateError(f"{STATE_YAML} not found")
    return load_yaml_text(STATE_YAML.read_text(encoding="utf-8"))


def load_schema() -> dict:
    import json

    with SCHEMA_PATH.open("r", encoding="utf-8") as f:
        return json.load(f)


def validate_schema(data: dict) -> list[str]:
    validator = Draft7Validator(load_schema())
    errors = []
    for err in sorted(validator.iter_errors(data), key=lambda e: e.path):
        loc = ".".join(str(p) for p in err.path) or "<root>"
        errors.append(f"schema: {loc}: {err.message}")
    return errors


def validate_business_rules(data: dict) -> list[str]:
    errors: list[str] = []
    status = data.get("status")
    task = data.get("task") or {}
    steps = data.get("steps") or []
    step_ids = [s["id"] for s in steps]

    if status == "done":
        if task.get("roadmap_id") is not None or task.get("title") is not None:
            errors.append("business: status == done requires task.roadmap_id/title == null")
        if data.get("whitelist"):
            errors.append("business: status == done requires an empty whitelist")
        if data.get("dod") is not None:
            errors.append("business: status == done requires dod == null")
        if steps:
            errors.append("business: status == done requires an empty steps list")
        if data.get("current_step") is not None:
            errors.append("business: status == done requires current_step == null")
        if data.get("next_action") is not None:
            errors.append("business: status == done requires next_action == null")
        if data.get("fail_counter", 0) != 0:
            errors.append("business: status == done requires fail_counter == 0")
        if data.get("failed_attempts"):
            errors.append("business: status == done requires an empty failed_attempts list")

    elif status == "in_progress":
        if not task.get("roadmap_id") or not task.get("title"):
            errors.append("business: status == in_progress requires task.roadmap_id and task.title")
        if not data.get("dod"):
            errors.append("business: status == in_progress requires a non-empty dod")
        if not steps:
            errors.append("business: status == in_progress requires at least one step")
        if len(step_ids) != len(set(step_ids)):
            errors.append("business: steps[].id must be unique")
        current = data.get("current_step")
        if current is not None and current not in step_ids:
            errors.append(f"business: current_step={current!r} does not reference any steps[].id")
        fc, fcm = data.get("fail_counter", 0), data.get("fail_counter_max", 3)
        if fc >= fcm:
            errors.append(
                f"business: fail_counter ({fc}) >= fail_counter_max ({fcm}) — "
                "this is the AGENTS.md 'критический тупик' state; the session "
                "should have rolled back before this was committed"
            )

    errors += validate_plan(data.get("plan"))
    return errors


def validate_plan(plan: dict | None) -> list[str]:
    if not plan:
        return []
    errors: list[str] = []
    ids = [i["id"] for i in plan["items"]]
    if len(ids) != len(set(ids)):
        errors.append("business: plan.items[].id must be unique")
    active = [i["id"] for i in plan["items"] if i["status"] == "in_progress"]
    if len(active) > 1:
        errors.append(f"business: plan has several in_progress items {active} — at most one allowed")
    return errors


PLAN_MARKS = {"done": "x", "partial": "~", "in_progress": ">", "todo": " ", "canceled": "-"}


def render_plan(plan: dict | None) -> str:
    if not plan:
        return ""
    lines = ["", f"## План: {plan['title']}"]
    if plan.get("source"):
        lines.append(f"_Источник: {plan['source']}_")
    lines.append("")
    for item in plan["items"]:
        line = f"- [{PLAN_MARKS[item['status']]}] **{item['id']}.** {item['description']}"
        note = item.get("note")
        if isinstance(note, str) and note:
            line += f" — {note}"
        lines.append(line)
        # Список строк → маркированный список под пунктом (читаемость для человека).
        if isinstance(note, list):
            lines.extend(f"  - {n}" for n in note)
    lines.append("")
    lines.append("Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато · [-] отменено")
    return "\n".join(lines) + "\n"


def render_markdown(data: dict) -> str:
    status = data["status"]
    if status == "done":
        return (
            GENERATED_HEADER
            + "\n# Состояние сессии\n\n"
            + "- **Текущая задача:** Нет (все шаги завершены)\n"
            + "- **Состояние:** done\n"
            + render_plan(data.get("plan"))
        )

    task = data["task"]
    lines = [GENERATED_HEADER, "\n# Текущая микро-сессия\n"]
    lines.append(f"- **Задача из ROADMAP:** {task['roadmap_id']} — {task['title']}")
    lines.append("- **Вайтлист файлов в работе (Изменяемые файлы):**")
    for path in data.get("whitelist", []):
        lines.append(f"  - {path}")
    lines.append(f"- **Критерий успеха (Definition of Done):** {data['dod']}")
    lines.append("")
    lines.append("## Итерационный трекер")
    # Пункты списка (`- `), иначе Markdown склеивает строки трекера в один абзац;
    # текущий незавершённый шаг — `[>]` и жирным, чтобы было видно, над чем идёт работа.
    current = data.get("current_step")
    for step in data["steps"]:
        if step["done"]:
            lines.append(f"- [x] Шаг {step['id']}: {step['description']}")
        elif str(step["id"]) == str(current):
            lines.append(f"- [>] **Шаг {step['id']}: {step['description']}**")
        else:
            lines.append(f"- [ ] Шаг {step['id']}: {step['description']}")
    lines.append("")
    lines.append("Легенда: [x] сделано · [>] текущий шаг · [ ] не начато")
    lines.append("")
    lines.append(f"- **Текущий шаг (current_step):** Шаг {data.get('current_step')}")
    lines.append(f"- **Следующий ход:** {data.get('next_action') or ''}")
    lines.append(
        f"- **Счетчик безуспешных компиляций:** {data.get('fail_counter', 0)}/{data.get('fail_counter_max', 3)}"
    )
    failed = data.get("failed_attempts") or []
    if failed:
        lines.append("- **Опробованные и неудачные подходы:**")
        for attempt in failed:
            lines.append(f"  - {attempt}")
    lines.append(f"- **Состояние:** {status}")
    lines.append("")
    return "\n".join(lines) + render_plan(data.get("plan"))


def git_changed_files() -> set[str]:
    """Paths with any working-tree/index change (modified, added, deleted,
    untracked, renamed), repo-relative with forward slashes. Uses `-z` for
    robust parsing (handles spaces/unicode without shell-quoting ambiguity).
    """
    proc = subprocess.run(
        ["git", "status", "--porcelain=v1", "-z"],
        cwd=ROOT,
        capture_output=True,
        check=True,
    )
    tokens = proc.stdout.split(b"\0")
    files: set[str] = set()
    i = 0
    while i < len(tokens):
        entry = tokens[i]
        i += 1
        if not entry:
            continue
        status_code = entry[:2].decode("utf-8", "replace")
        path = entry[3:].decode("utf-8", "replace").replace("\\", "/")
        files.add(path)
        if status_code[0] in ("R", "C"):
            # Rename/copy: the next NUL-separated token is the orig path.
            i += 1
    return files


def cmd_check_whitelist() -> int:
    data = load_state()
    errors = validate_schema(data) + validate_business_rules(data)
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        return 1

    if data.get("status") != "in_progress":
        print(f"status == {data.get('status')!r} — whitelist check skipped (nothing to constrain).")
        return 0

    whitelist = {p.replace("\\", "/") for p in data.get("whitelist", [])}
    changed = git_changed_files()
    offenders = sorted(
        f for f in changed
        if f not in whitelist and f not in WHITELIST_EXEMPT and not WHITELIST_EXEMPT_RE.match(f)
    )

    if offenders:
        print("Изменены файлы вне вайтлиста _STATE_.yaml:", file=sys.stderr)
        for f in offenders:
            print(f"  {f}", file=sys.stderr)
        print(f"\nВайтлист ({data['task']['roadmap_id']}): {sorted(whitelist) or '[]'}", file=sys.stderr)
        print(
            "\nЛибо верните файл(ы) в исходное состояние, либо расширьте "
            "whitelist в _STATE_.yaml, либо запросите разрешение пользователя "
            "(docs/00_workflow_v1.0/execution.md §1).",
            file=sys.stderr,
        )
        return 1

    print(f"OK — все изменённые файлы ({len(changed)}) входят в whitelist или исключены из проверки.")
    return 0


def cmd_step_done(args: list[str]) -> int:
    """Успешное завершение current_step (execution.md §4): отмечает шаг
    done:true, сдвигает current_step на следующий элемент steps[] (порядок
    списка, не id+1 — execution.md §1 допускает несмежные id после
    разбиения на подшаги), сбрасывает fail_counter/failed_attempts, пишет
    _STATE_.md и печатает строку для чата. После последнего шага
    current_step не меняется и next_action становится «закрытие по
    closing.md» независимо от --next (его можно не передавать).
    """
    next_action_arg: str | None = None
    if args:
        if len(args) == 2 and args[0] == "--next" and args[1]:
            next_action_arg = args[1]
        else:
            print('usage: step-done [--next "<next_action>"]', file=sys.stderr)
            return 2

    data = load_state()
    errors = validate_schema(data) + validate_business_rules(data)
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        return 1

    if data.get("status") != "in_progress":
        print("error: step-done requires status == in_progress", file=sys.stderr)
        return 1

    steps = data.get("steps") or []
    current = data.get("current_step")
    idx = next((i for i, s in enumerate(steps) if s["id"] == current), None)
    if idx is None:
        print(f"error: current_step={current!r} does not reference any steps[].id", file=sys.stderr)
        return 1
    if steps[idx]["done"]:
        print(f"error: step {current} is already marked done", file=sys.stderr)
        return 1

    is_last = idx + 1 >= len(steps)
    if not is_last and next_action_arg is None:
        print('error: --next "<next_action>" is required — this is not the last step', file=sys.stderr)
        return 2

    text = STATE_YAML.read_text(encoding="utf-8")

    id_marker = f"- id: {current}\n"
    id_pos = text.find(id_marker)
    if id_pos == -1:
        print(f"error: could not locate '- id: {current}' line in {STATE_YAML.name}", file=sys.stderr)
        return 1
    done_pos = text.find("done: false", id_pos)
    if done_pos == -1:
        print(f"error: could not locate 'done: false' for step {current} in {STATE_YAML.name}", file=sys.stderr)
        return 1
    text = text[:done_pos] + "done: true" + text[done_pos + len("done: false"):]

    if is_last:
        new_current = current
        new_next_action = "закрытие по closing.md"
    else:
        new_current = steps[idx + 1]["id"]
        new_next_action = next_action_arg

    text, n = re.subn(r"(?m)^current_step: .*$", f"current_step: {new_current}", text, count=1)
    if n != 1:
        print("error: could not locate 'current_step:' line", file=sys.stderr)
        return 1
    text, n = re.subn(
        r"(?m)^next_action: .*$",
        f"next_action: {json.dumps(new_next_action, ensure_ascii=False)}",
        text,
        count=1,
    )
    if n != 1:
        print("error: could not locate 'next_action:' line", file=sys.stderr)
        return 1
    text, n = re.subn(r"(?m)^fail_counter: \d+$", "fail_counter: 0", text, count=1)
    if n != 1:
        print("error: could not locate 'fail_counter:' line", file=sys.stderr)
        return 1
    # failed_attempts может быть "[]" или многострочным списком — съедаем всё
    # до следующего поля верхнего уровня (следующая строка без отступа) или
    # до конца файла.
    text, n = re.subn(r"(?ms)^failed_attempts:.*?(?=\n\S|\Z)", "failed_attempts: []", text, count=1)
    if n != 1:
        print("error: could not locate 'failed_attempts:' field", file=sys.stderr)
        return 1

    new_data_for_check = load_yaml_text(text)
    errors = validate_schema(new_data_for_check) + validate_business_rules(new_data_for_check)
    if errors:
        print("error: resulting _STATE_.yaml would be invalid, aborting write:", file=sys.stderr)
        for e in errors:
            print(e, file=sys.stderr)
        return 1

    STATE_YAML.write_text(text, encoding="utf-8", newline="\n")
    STATE_MD.write_text(render_markdown(new_data_for_check), encoding="utf-8", newline="\n")

    if is_last:
        print(f"Шаг {current} завершён (последний шаг) — закрытие по closing.md")
    else:
        print(f"Шаг {current} завершён, текущий — {new_current}")
    return 0


def cmd_render() -> int:
    data = load_state()
    errors = validate_schema(data) + validate_business_rules(data)
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        print(f"\n{STATE_YAML.name} is invalid — {STATE_MD.name} was NOT regenerated.", file=sys.stderr)
        return 1
    STATE_MD.write_text(render_markdown(data), encoding="utf-8", newline="\n")
    print(f"Wrote {STATE_MD.relative_to(ROOT)}")
    return 0


def cmd_check() -> int:
    data = load_state()
    errors = validate_schema(data) + validate_business_rules(data)
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        return 1
    expected = render_markdown(data)
    actual = STATE_MD.read_text(encoding="utf-8") if STATE_MD.exists() else None
    if actual != expected:
        print(
            f"{STATE_MD.name} is stale (does not match _STATE_.yaml).\n"
            "Run: python tools/state_tool.py render",
            file=sys.stderr,
        )
        return 1
    print("_STATE_.yaml valid, _STATE_.md up to date.")
    return 0


COMMANDS = {
    "render": cmd_render,
    "check": cmd_check,
    "check-whitelist": cmd_check_whitelist,
}


def _force_utf8_console() -> None:
    # Windows terminals (cmd.exe, some PowerShell hosts) default to a legacy
    # codepage that mangles Cyrillic output; force UTF-8 regardless of host.
    for stream in (sys.stdout, sys.stderr):
        if getattr(stream, "encoding", "").lower() != "utf-8":
            try:
                stream.reconfigure(encoding="utf-8")
            except (AttributeError, OSError):
                pass


def main(argv: list[str]) -> int:
    _force_utf8_console()
    if not argv:
        print(__doc__)
        return 2
    cmd, rest = argv[0], argv[1:]
    try:
        if cmd == "step-done":
            return cmd_step_done(rest)
        if rest or cmd not in COMMANDS:
            print(__doc__)
            return 2
        return COMMANDS[cmd]()
    except StateError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
