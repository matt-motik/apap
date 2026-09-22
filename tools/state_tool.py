#!/usr/bin/env python3
"""_STATE_.yaml <-> _STATE_.md tooling.

_STATE_.yaml is the source of truth for the current micro-session
(see AGENTS.md, Шаг 0/3/4). _STATE_.md is a generated, human/agent-readable
rendering of it — it must never be hand-edited; `render` regenerates it and
`check` fails if it has drifted from what `render` would produce.

Usage:
    python tools/state_tool.py render            # (re)generate _STATE_.md from _STATE_.yaml
    python tools/state_tool.py check              # validate schema + business rules,
                                                   # fail if _STATE_.md is stale
    python tools/state_tool.py check-whitelist    # fail if any changed file (git status)
                                                   # is outside _STATE_.yaml's whitelist
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import yaml
from jsonschema import Draft7Validator

# Files that are part of the workflow bookkeeping itself, not "code" — they
# legitimately change on nearly every commit regardless of the task's
# whitelist (see AGENTS.md Шаг 4.4: _STATE_.md is committed together with
# the code on every successful step; ROADMAP.md is patched at Шаг 5).
WHITELIST_EXEMPT = {"_STATE_.md", "_STATE_.yaml", "ROADMAP.md"}

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


def load_state() -> dict:
    if not STATE_YAML.exists():
        raise StateError(f"{STATE_YAML} not found")
    with STATE_YAML.open("r", encoding="utf-8") as f:
        data = yaml.safe_load(f) or {}
    return data


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

    return errors


def render_markdown(data: dict) -> str:
    status = data["status"]
    if status == "done":
        return (
            GENERATED_HEADER
            + "\n# Состояние сессии\n\n"
            + "- **Текущая задача:** Нет (все шаги завершены)\n"
            + "- **Состояние:** done\n"
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
    for step in data["steps"]:
        mark = "x" if step["done"] else " "
        lines.append(f"[{mark}] Шаг {step['id']}: {step['description']}")
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
    return "\n".join(lines)


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
    offenders = sorted(f for f in changed if f not in whitelist and f not in WHITELIST_EXEMPT)

    if offenders:
        print("Изменены файлы вне вайтлиста _STATE_.yaml:", file=sys.stderr)
        for f in offenders:
            print(f"  {f}", file=sys.stderr)
        print(f"\nВайтлист ({data['task']['roadmap_id']}): {sorted(whitelist) or '[]'}", file=sys.stderr)
        print(
            "\nЛибо верните файл(ы) в исходное состояние, либо расширьте "
            "whitelist в _STATE_.yaml, либо запросите разрешение пользователя "
            "(AGENTS.md Шаг 4.1, СТРОГОЕ ТАБУ).",
            file=sys.stderr,
        )
        return 1

    print(f"OK — все изменённые файлы ({len(changed)}) входят в whitelist или исключены из проверки.")
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
    if len(argv) != 1 or argv[0] not in COMMANDS:
        print(__doc__)
        return 2
    try:
        return COMMANDS[argv[0]]()
    except StateError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
