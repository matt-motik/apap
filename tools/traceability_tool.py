#!/usr/bin/env python3
"""SPEC <-> ROADMAP traceability checker (AGENTS.md §3, "Сквозная
трассируемость"; executable-workflow plan item 3, folding in item 7's first
two bullets: "every task -> valid spec anchor", "every spec prefix ->
unique").

Validates, from ROADMAP.md and docs/spec_*.md, without touching either file:

  1. Every prefix in the registry table (ROADMAP.md header) is unique and
     points at a docs/ file that exists.
  2. Each spec file's own self-declared prefix (if any) matches the
     registry — mismatch is an error, absence is a warning (older specs
     predate the convention).
  3. Every task ID in a ROADMAP task table matches
     `<registered prefix>-<paragraph>[.subtask]` (or the `-B<N>` bug-round
     convention already in use) and its prefix is registered.
  4. Every `docs/...md#anchor` link in a "ТЗ"/"Спека" column resolves: the
     file exists and a heading in it slugifies to that anchor (GitHub-style
     slugification). External (http/https) links are ignored.
  5. Every row whose Статус contains "✅" names a commit that actually
     exists in this repo's history (`git cat-file -e <hash>^{commit}`).
     Rows explicitly marked "в рабочем дереве" / "подтверждено пользователем"
     (no commit expected) are exempted; anything else with ✅ and no
     hash-like token is a warning, not a hard error (pre-existing rows).

Usage:
    python tools/traceability_tool.py check
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROADMAP = ROOT / "ROADMAP.md"

REGISTRY_ROW_RE = re.compile(r"^>\s*\|\s*`(docs/[^`]+)`\s*\|\s*`([^`]+)`\s*\|\s*$")
SELF_PREFIX_RE = re.compile(r"Префикс[^`\n]*`([^`]+)`", re.IGNORECASE)
DOC_LINK_RE = re.compile(r"\[[^\]]*\]\((docs/[^)#\s]+)(#[^)\s]+)?\)")
HASH_TOKEN_RE = re.compile(r"`?\b([0-9a-f]{7,40})\b`?")
NO_COMMIT_EXPECTED_RE = re.compile(r"рабочем дереве|подтверждено пользователем", re.IGNORECASE)
ID_RE = re.compile(r"^([A-Za-z][A-Za-z0-9]*\.[0-9]+)-([0-9]+(?:\.[0-9]+)*|B[0-9]+)$")


class Result:
    def __init__(self) -> None:
        self.errors: list[str] = []
        self.warnings: list[str] = []

    def error(self, msg: str) -> None:
        self.errors.append(msg)

    def warn(self, msg: str) -> None:
        self.warnings.append(msg)

    @property
    def ok(self) -> bool:
        return not self.errors


def slugify(heading: str) -> str:
    """Approximate GitHub's heading -> anchor algorithm (see AGENTS.md §3,
    "Стиль якоря"). GitHub does NOT collapse consecutive whitespace before
    converting to hyphens — removing a punctuation char like em-dash that
    sits between two spaces ("X — Y") leaves two adjacent spaces, which
    become two adjacent hyphens ("x--y"), not one."""
    s = heading.strip().lower()
    s = re.sub(r"[^\w\s-]", "", s, flags=re.UNICODE)
    s = re.sub(r"\s", "-", s)
    return s


def heading_slugs(md_path: Path) -> set[str]:
    slugs: set[str] = set()
    for line in md_path.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^#{1,6}\s+(.*)$", line)
        if m:
            slugs.add(slugify(m.group(1)))
    return slugs


def parse_registry(res: Result) -> dict[str, str]:
    """Returns {prefix: spec_relpath}, recording duplicate-prefix errors."""
    text = ROADMAP.read_text(encoding="utf-8")
    prefix_to_spec: dict[str, str] = {}
    for line in text.splitlines():
        m = REGISTRY_ROW_RE.match(line)
        if not m:
            continue
        spec_path, prefix = m.group(1), m.group(2)
        if prefix in prefix_to_spec and prefix_to_spec[prefix] != spec_path:
            res.error(
                f"registry: prefix '{prefix}' is registered for both "
                f"'{prefix_to_spec[prefix]}' and '{spec_path}' — prefixes must be unique"
            )
            continue
        prefix_to_spec[prefix] = spec_path
        if not (ROOT / spec_path).exists():
            res.error(f"registry: '{spec_path}' (prefix '{prefix}') does not exist")
    return prefix_to_spec


def check_self_declared_prefixes(res: Result, registry: dict[str, str]) -> None:
    for prefix, spec_path in registry.items():
        full = ROOT / spec_path
        if not full.exists():
            continue
        head = "\n".join(full.read_text(encoding="utf-8").splitlines()[:20])
        m = SELF_PREFIX_RE.search(head)
        if not m:
            res.warn(f"{spec_path}: no self-declared 'Префикс' found in the first 20 lines (registry says '{prefix}')")
        elif m.group(1) != prefix:
            res.error(
                f"{spec_path}: self-declared prefix '{m.group(1)}' does not match "
                f"registry prefix '{prefix}'"
            )


def check_doc_link(res: Result, context: str, path: str, anchor: str | None) -> None:
    full = ROOT / path
    if not full.exists():
        res.error(f"{context}: link target '{path}' does not exist")
        return
    if anchor is None:
        return
    anchor = anchor.lstrip("#")
    slugs = heading_slugs(full)
    if anchor not in slugs:
        res.error(f"{context}: anchor '#{anchor}' not found in '{path}' (no heading slugifies to it)")


def commit_exists(hash_: str) -> bool:
    proc = subprocess.run(
        ["git", "cat-file", "-e", f"{hash_}^{{commit}}"],
        cwd=ROOT,
        capture_output=True,
    )
    return proc.returncode == 0


def check_status_commit(res: Result, context: str, status: str) -> None:
    if "✅" not in status:
        return
    if NO_COMMIT_EXPECTED_RE.search(status):
        return
    m = HASH_TOKEN_RE.search(status)
    if not m:
        res.warn(f"{context}: status marked ✅ but no commit hash found in '{status.strip()}'")
        return
    h = m.group(1)
    if not commit_exists(h):
        res.error(f"{context}: status references commit '{h}' which does not exist in this repo's history")


def split_pipe_row(line: str) -> list[str]:
    line = line.strip()
    if line.startswith("|"):
        line = line[1:]
    if line.endswith("|"):
        line = line[:-1]
    return [cell.strip() for cell in line.split("|")]


def find_tables(text: str) -> list[list[list[str]]]:
    """Returns a list of tables; each table is a list of rows (header +
    data), each row a list of cell strings. Skips the blockquoted registry
    table (handled separately) by only matching lines not starting with '>'.
    """
    lines = text.splitlines()
    tables = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.strip().startswith("|") and i + 1 < len(lines) and re.match(r"^\s*\|[\s:|-]+\|\s*$", lines[i + 1]):
            header = split_pipe_row(line)
            rows = [header]
            i += 2  # skip separator row
            while i < len(lines) and lines[i].strip().startswith("|"):
                rows.append(split_pipe_row(lines[i]))
                i += 1
            tables.append(rows)
            continue
        i += 1
    return tables


def check_task_table(res: Result, rows: list[list[str]], registry: dict[str, str]) -> None:
    header = rows[0]
    try:
        id_idx = header.index("ID")
        status_idx = header.index("Статус")
        tz_idx = header.index("ТЗ")
    except ValueError:
        return  # not a task table
    for row in rows[1:]:
        if len(row) <= max(id_idx, status_idx, tz_idx):
            continue
        task_id, status, tz = row[id_idx], row[status_idx], row[tz_idx]
        context = f"ROADMAP task {task_id}"
        m = ID_RE.match(task_id)
        if not m:
            res.error(f"{context}: ID does not match '<PREFIX>-<paragraph>[.sub]' or '<PREFIX>-B<n>'")
        elif m.group(1) not in registry:
            res.error(f"{context}: prefix '{m.group(1)}' is not registered in the ROADMAP.md header")
        for link_m in DOC_LINK_RE.finditer(tz):
            check_doc_link(res, context, link_m.group(1), link_m.group(2))
        check_status_commit(res, context, status)


def check_microfix_table(res: Result, rows: list[list[str]]) -> None:
    header = rows[0]
    if "Спека" not in header or "Коммит" not in header:
        return
    spec_idx = header.index("Спека")
    commit_idx = header.index("Коммит")
    date_idx = header.index("Дата") if "Дата" in header else 0
    for row in rows[1:]:
        if len(row) <= max(spec_idx, commit_idx):
            continue
        spec_cell, commit_cell = row[spec_idx], row[commit_idx]
        context = f"ROADMAP micro-fix {row[date_idx]}"
        for link_m in DOC_LINK_RE.finditer(spec_cell):
            check_doc_link(res, context, link_m.group(1), link_m.group(2))
        commit_cell_clean = commit_cell.strip("` ")
        if NO_COMMIT_EXPECTED_RE.search(commit_cell) or "рабочее дерево" in commit_cell.lower():
            continue
        m = HASH_TOKEN_RE.search(commit_cell)
        if m and not commit_exists(m.group(1)):
            res.error(f"{context}: commit '{m.group(1)}' does not exist in this repo's history")


def cmd_check() -> int:
    res = Result()
    registry = parse_registry(res)
    check_self_declared_prefixes(res, registry)

    text = ROADMAP.read_text(encoding="utf-8")
    for table in find_tables(text):
        check_task_table(res, table, registry)
        check_microfix_table(res, table)

    for w in res.warnings:
        print(f"WARN  {w}")
    for e in res.errors:
        print(f"ERROR {e}", file=sys.stderr)

    if not res.ok:
        print(f"\n{len(res.errors)} error(s), {len(res.warnings)} warning(s).", file=sys.stderr)
        return 1
    print(f"OK — {len(res.warnings)} warning(s), 0 errors.")
    return 0


def _force_utf8_console() -> None:
    for stream in (sys.stdout, sys.stderr):
        if getattr(stream, "encoding", "").lower() != "utf-8":
            try:
                stream.reconfigure(encoding="utf-8")
            except (AttributeError, OSError):
                pass


def main(argv: list[str]) -> int:
    _force_utf8_console()
    if len(argv) != 1 or argv[0] != "check":
        print(__doc__)
        return 2
    return cmd_check()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
