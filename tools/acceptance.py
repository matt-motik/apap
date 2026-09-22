#!/usr/bin/env python3
"""Cross-OS spec acceptance (executable-workflow plan item 5).

Регламент (коротко):
  * Приёмка — отдельный этап, НЕ после каждой микро-задачи. Запускается,
    когда все задачи спеки в ROADMAP закрыты и замечаний нет.
  * Пакет приёмки = каталог docs/acceptance/<name>/ с criteria.yaml (что
    проверяем) и results-<os>.yaml (что получилось на этой ОС). Файлы
    результатов у каждой ОС свои — прогоны на разных машинах не конфликтуют.
  * На каждой машине, в любом порядке и в любое время:
        git pull
        python tools/acceptance.py run <name>
        git push
    `run` сам коммитит results-<os>.yaml с подробным сообщением (замеры,
    провалы, находки) — ручной коммит не нужен, контекст не теряется.
  * Пакет принят, когда на всех `platforms` из criteria.yaml есть зелёный,
    не устаревший прогон. Прогон устаревает, если после его коммита
    изменились `code_paths` (src/, tests/, Cargo.* ...) — тогда эту ОС
    прогоняют заново; остальные сохраняются, если код не менялся.
  * Классы критериев:
        hard   — провал = прогон красный (корректность, 0 аллокаций, память);
        perf   — зависит от машины: провал = «находка» с замерами. Находка
                 блокирует приёмку, пока у критерия не указан known_issue
                 (ID задачи ROADMAP) — т.е. пока пользователь не решил,
                 чинить или принять.
  * Виды проверок критерия (можно сочетать):
        tests  — шаблоны имён тестов "<suite>/<glob>" (suite по умолчанию
                 debug); критерий зелёный, если все совпавшие тесты ok и
                 совпал хотя бы один (переименованный тест = провал, а не
                 молчаливый пропуск);
        suites — целиком зелёный набор (clippy, tooling, ...);
        manual — вопрос пользователю в конце прогона (y/n/s);
                 scope each — на каждой ОС, any — достаточно одной;
                 evidence — уже подтверждено ранее (ссылка), не спрашивать;
        review — платформонезависимо, обязателен evidence.
  * requires_env: часть тестов молча выходит (ok), если не задан путь к
    файлу-образцу (MUSIC_*_TEST_FILE). Такой критерий без переменной —
    pending, а не pass. Пути у каждой машины свои: tools/acceptance.local.yaml
    (не в git, образец — tools/acceptance.local.example.yaml).

Usage:
    python tools/acceptance.py run <name> [--skip-manual] [--no-commit]
    python tools/acceptance.py status [<name>]
    python tools/acceptance.py check                 # structure of all packages
"""
from __future__ import annotations

import argparse
import datetime as dt
import fnmatch
import os
import platform
import re
import subprocess
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
ACC_DIR = ROOT / "docs" / "acceptance"
LOCAL_CFG = ROOT / "tools" / "acceptance.local.yaml"

KNOWN_PLATFORMS = ("linux", "windows", "macos")
CLASSES = ("hard", "perf")
TEST_LINE_RE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\s*$")
SHOW_OUTPUT_RE = re.compile(r"^---- (\S+) stdout ----$")


# ----------------------------------------------------------------- helpers

def current_platform() -> str:
    sysname = platform.system().lower()
    return {"darwin": "macos"}.get(sysname, sysname)


def git(*args: str, check: bool = True) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, encoding="utf-8", check=check
    ).stdout.strip()


def cpu_model() -> str:
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
    if sys.platform == "darwin":
        try:
            return subprocess.run(
                ["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True
            ).stdout.strip()
        except OSError:
            pass
    return platform.processor() or "unknown"


def host_info() -> dict:
    try:
        rustc = subprocess.run(["rustc", "-V"], capture_output=True, text=True).stdout.strip()
    except OSError:
        rustc = "unknown"
    return {
        "os": f"{platform.system()} {platform.release()}",
        "machine": platform.node(),
        "cpu": cpu_model(),
        "cores": os.cpu_count(),
        "rustc": rustc,
    }


def load_yaml(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        return yaml.safe_load(f) or {}


def package_dir(name: str) -> Path:
    return ACC_DIR / name


def results_path(name: str, plat: str) -> Path:
    return package_dir(name) / f"results-{plat}.yaml"


def effective_env() -> dict[str, str]:
    """os.environ + env: из tools/acceptance.local.yaml (переменная окружения
    важнее файла). Пустые значения из файла игнорируются."""
    env = dict(os.environ)
    if LOCAL_CFG.exists():
        for k, v in ((load_yaml(LOCAL_CFG).get("env")) or {}).items():
            if v and k not in os.environ:
                env[k] = str(v)
    return env


def list_packages() -> list[str]:
    if not ACC_DIR.exists():
        return []
    return sorted(p.name for p in ACC_DIR.iterdir() if (p / "criteria.yaml").exists())


# ----------------------------------------------------------------- structure check

def validate_package(name: str) -> list[str]:
    errs: list[str] = []
    try:
        pkg = load_yaml(package_dir(name) / "criteria.yaml")
    except (OSError, yaml.YAMLError) as e:
        return [f"{name}: cannot read criteria.yaml: {e}"]

    for key in ("title", "platforms", "code_paths", "suites", "criteria"):
        if key not in pkg:
            errs.append(f"{name}: missing top-level key '{key}'")
    if errs:
        return errs

    for plat in pkg["platforms"]:
        if plat not in KNOWN_PLATFORMS:
            errs.append(f"{name}: unknown platform '{plat}' (known: {KNOWN_PLATFORMS})")

    suites = pkg["suites"]
    for sid, s in suites.items():
        if not isinstance(s, dict) or not s.get("cmd"):
            errs.append(f"{name}: suite '{sid}' needs a non-empty cmd list")

    seen: set[str] = set()
    for c in pkg["criteria"]:
        cid = c.get("id", "<no id>")
        where = f"{name}: criterion {cid}"
        if cid in seen:
            errs.append(f"{where}: duplicate id")
        seen.add(cid)
        if not c.get("text"):
            errs.append(f"{where}: missing text")
        if c.get("class", "hard") not in CLASSES:
            errs.append(f"{where}: class must be one of {CLASSES}")
        has_check = False
        for pat in c.get("tests") or []:
            has_check = True
            suite = pat.split("/", 1)[0] if "/" in pat else "debug"
            if suite not in suites:
                errs.append(f"{where}: tests pattern '{pat}' references unknown suite '{suite}'")
        for sid in c.get("suites") or []:
            has_check = True
            if sid not in suites:
                errs.append(f"{where}: unknown suite '{sid}'")
        manual = c.get("manual")
        if manual:
            has_check = True
            if not manual.get("ask") and not manual.get("evidence"):
                errs.append(f"{where}: manual needs 'ask' or 'evidence'")
            if manual.get("scope", "each") not in ("each", "any"):
                errs.append(f"{where}: manual.scope must be each|any")
        if "review" in c:
            has_check = True
            if not (c["review"] or {}).get("evidence"):
                errs.append(f"{where}: review needs evidence")
        for v in c.get("requires_env") or []:
            if not re.fullmatch(r"[A-Z][A-Z0-9_]*", v):
                errs.append(f"{where}: requires_env entry '{v}' is not an env var name")
        if not has_check:
            errs.append(f"{where}: no tests/suites/manual/review — nothing verifies it")
        for plat in c.get("platforms") or []:
            if plat not in pkg["platforms"]:
                errs.append(f"{where}: platform '{plat}' not in package platforms")
    return errs


def cmd_check(_args) -> int:
    pkgs = list_packages()
    errs = [e for p in pkgs for e in validate_package(p)]
    for e in errs:
        print(f"ERROR {e}", file=sys.stderr)
    if errs:
        return 1
    print(f"OK — {len(pkgs)} acceptance package(s) structurally valid.")
    return 0


# ----------------------------------------------------------------- run

def run_suite(sid: str, suite: dict, env: dict[str, str]) -> dict:
    cmd = [sys.executable if part == "{python}" else part for part in suite["cmd"]]
    print(f"\n=== suite {sid}: {' '.join(suite['cmd'])}", flush=True)
    started = dt.datetime.now()
    proc = subprocess.Popen(
        cmd, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, encoding="utf-8", errors="replace",
    )
    lines: list[str] = []
    assert proc.stdout is not None
    for line in proc.stdout:
        lines.append(line.rstrip("\n"))
        if TEST_LINE_RE.match(line.strip()) or line.startswith(("error", "warning", "ERROR", "test result")):
            print(f"  {line.rstrip()}", flush=True)
    rc = proc.wait()
    secs = (dt.datetime.now() - started).total_seconds()

    tests: dict[str, str] = {}
    metrics: dict[str, list[str]] = {}
    current: str | None = None
    for raw in lines:
        line = raw.strip()
        m = TEST_LINE_RE.match(line)
        if m:
            name, st = m.groups()
            # Same test name in several binaries: any failure wins.
            if tests.get(name) != "FAILED":
                tests[name] = st
            continue
        m = SHOW_OUTPUT_RE.match(line)
        if m:
            current = m.group(1)
            continue
        if line in ("successes:", "failures:") or line.startswith("test result"):
            current = None
            continue
        if current and line.startswith(f"{current}:"):
            metrics.setdefault(current, []).append(line[len(current) + 1:].strip())

    tail = [l for l in lines if l.strip()][-25:] if rc != 0 else []
    print(f"=== suite {sid}: rc={rc}, {len(tests)} tests parsed, {secs:.0f}s", flush=True)
    return {"rc": rc, "tests": tests, "metrics": metrics, "secs": secs, "tail": tail}


def evaluate_tests(patterns: list[str], suite_runs: dict) -> tuple[str, list[str], list[str]]:
    """-> (status, problems, metrics)"""
    problems: list[str] = []
    metrics: list[str] = []
    for pat in patterns:
        suite, glob = pat.split("/", 1) if "/" in pat else ("debug", pat)
        run = suite_runs[suite]
        matched = [n for n in run["tests"] if fnmatch.fnmatchcase(n, glob)]
        if not matched:
            problems.append(f"{pat}: ни один тест не совпал (переименован/удалён?)")
            continue
        for n in matched:
            st = run["tests"][n]
            if st == "FAILED":
                problems.append(f"{suite}/{n}: FAILED")
            elif st == "ignored":
                problems.append(f"{suite}/{n}: ignored (запустите набор с --include-ignored)")
            metrics += [f"{n}: {m}" for m in run["metrics"].get(n, [])]
    return ("fail" if problems else "pass"), problems, metrics


def ask_manual(cid: str, text: str, ask: str) -> tuple[str, str]:
    print(f"\n[{cid}] {text}\n    Проверка: {ask}")
    while True:
        try:
            ans = input("    Результат? [y] ок / [n] провал / [s] пропустить: ").strip().lower()
        except EOFError:
            return "pending", "нет интерактивного ввода"
        if ans in ("y", "n", "s"):
            break
    note = ""
    if ans == "n":
        try:
            note = input("    Что не так (одной строкой): ").strip()
        except EOFError:
            pass
    return {"y": "pass", "n": "fail", "s": "pending"}[ans], note


def criterion_applies(c: dict, plat: str) -> bool:
    return not c.get("platforms") or plat in c["platforms"]


def cmd_run(args) -> int:
    name = args.name
    errs = validate_package(name)
    if errs:
        for e in errs:
            print(f"ERROR {e}", file=sys.stderr)
        return 1
    pkg = load_yaml(package_dir(name) / "criteria.yaml")
    plat = current_platform()
    if plat not in pkg["platforms"]:
        print(f"Платформа '{plat}' не входит в platforms пакета {pkg['platforms']}.", file=sys.stderr)
        return 1

    dirty = git("status", "--porcelain", "--", *pkg["code_paths"])
    if dirty and not args.no_commit:
        print(
            "Незакоммиченные изменения в code_paths — результат не будет соответствовать "
            f"ни одному коммиту:\n{dirty}\nЗакоммитьте их или используйте --no-commit.",
            file=sys.stderr,
        )
        return 1
    commit = git("rev-parse", "HEAD")

    criteria = [c for c in pkg["criteria"] if criterion_applies(c, plat)]
    needed = set()
    for c in criteria:
        needed |= set(c.get("suites") or [])
        needed |= {(p.split("/", 1)[0] if "/" in p else "debug") for p in c.get("tests") or []}
    env = effective_env()
    suite_runs = {sid: run_suite(sid, pkg["suites"][sid], env) for sid in pkg["suites"] if sid in needed}

    # Manual answers already given by another OS count for scope: any.
    other_manual: dict[str, dict] = {}
    for p in pkg["platforms"]:
        rp = results_path(name, p)
        if p != plat and rp.exists():
            for cid, r in (load_yaml(rp).get("criteria") or {}).items():
                if r.get("manual") == "pass":
                    other_manual[cid] = {"platform": p, "commit": load_yaml(rp).get("commit")}

    results: dict[str, dict] = {}
    for c in criteria:
        cid, cls = c["id"], c.get("class", "hard")
        r: dict = {"class": cls}
        problems: list[str] = []
        if c.get("tests"):
            st, pr, metrics = evaluate_tests(c["tests"], suite_runs)
            r["tests"] = st
            problems += pr
            if metrics:
                r["metrics"] = metrics
        for sid in c.get("suites") or []:
            if suite_runs[sid]["rc"] != 0:
                problems.append(f"suite {sid}: rc={suite_runs[sid]['rc']}")
        if c.get("suites"):
            r["suites"] = "fail" if any(suite_runs[s]["rc"] for s in c["suites"]) else "pass"
        if "review" in c:
            r["review"] = "pass"
        manual = c.get("manual")
        if manual:
            if manual.get("evidence"):
                r["manual"], r["manual_note"] = "pass", f"evidence: {manual['evidence']}"
            elif manual.get("scope") == "any" and cid in other_manual:
                src = other_manual[cid]
                r["manual"], r["manual_note"] = "pass", f"подтверждено на {src['platform']} @ {str(src['commit'])[:7]}"
            else:
                r["manual"], r["manual_note"] = "pending", ""
        missing_env = [v for v in c.get("requires_env") or [] if not env.get(v)]
        if missing_env:
            r["missing_env"] = missing_env
        results[cid] = r
        r["_problems"] = problems

    if not args.skip_manual:
        todo = [c for c in criteria if results[c["id"]].get("manual") == "pending"]
        if todo:
            print(f"\n=== ручная проверка: {len(todo)} пункт(ов) (запустите приложение: cargo run --release)")
        for c in todo:
            st, note = ask_manual(c["id"], c["text"], c["manual"]["ask"])
            results[c["id"]]["manual"] = st
            results[c["id"]]["manual_note"] = note
            if st == "fail":
                results[c["id"]]["_problems"].append(f"manual: {note or 'провал'}")

    # Final status per criterion.
    for c in criteria:
        r = results[c["id"]]
        problems = r.pop("_problems")
        if problems:
            if r["class"] == "perf":
                r["status"] = "finding"
                if c.get("known_issue"):
                    r["known_issue"] = c["known_issue"]
            else:
                r["status"] = "fail"
            r["problems"] = problems
        elif r.get("manual") == "pending" or r.get("missing_env"):
            r["status"] = "pending"
            if r.get("missing_env"):
                r["problems"] = [
                    f"не задан {v} — тест проходит вхолостую (tools/acceptance.local.yaml)"
                    for v in r["missing_env"]
                ]
        else:
            r["status"] = "pass"
        if not r.get("manual_note"):
            r.pop("manual_note", None)

    verdict = compute_verdict(results)
    doc = {
        "package": name,
        "platform": plat,
        "commit": commit,
        "date": dt.datetime.now().astimezone().isoformat(timespec="seconds"),
        "host": host_info(),
        "verdict": verdict,
        "suites": {
            sid: {"rc": s["rc"], "tests": len(s["tests"]), "seconds": round(s["secs"])}
            | ({"tail": s["tail"]} if s["tail"] else {})
            for sid, s in suite_runs.items()
        },
        "criteria": results,
    }
    out = results_path(name, plat)
    header = (
        f"# GENERATED by tools/acceptance.py run {name} — do not edit by hand.\n"
        f"# Регламент: см. docstring tools/acceptance.py; сводка: tools/acceptance.py status {name}\n"
    )
    out.write_text(header + yaml.safe_dump(doc, allow_unicode=True, sort_keys=False, width=100),
                   encoding="utf-8", newline="\n")

    message = commit_message(pkg, doc)
    print("\n" + message)
    if args.no_commit:
        print(f"\n--no-commit: {out.relative_to(ROOT)} записан, но не закоммичен.")
    else:
        git("add", str(out.relative_to(ROOT)))
        proc = subprocess.run(["git", "commit", "-q", "-F", "-"], cwd=ROOT, input=message,
                              text=True, encoding="utf-8")
        if proc.returncode != 0:
            print("git commit не удался — файл результатов оставлен в рабочем дереве.", file=sys.stderr)
            return 1
        print(f"\nЗакоммичено: {git('log', '--oneline', '-1')}\nДальше: git push")
    print()
    cmd_status(argparse.Namespace(name=name))
    return 0 if verdict in ("pass", "pass_with_known_issues") else 1


def compute_verdict(results: dict) -> str:
    sts = [r["status"] for r in results.values()]
    if "fail" in sts:
        return "fail"
    if any(r["status"] == "finding" and not r.get("known_issue") for r in results.values()):
        return "needs_decision"
    if "pending" in sts:
        return "incomplete"
    if "finding" in sts:
        return "pass_with_known_issues"
    return "pass"


def commit_message(pkg: dict, doc: dict) -> str:
    res = doc["criteria"]
    counts: dict[str, int] = {}
    for r in res.values():
        counts[r["status"]] = counts.get(r["status"], 0) + 1
    h = doc["host"]
    lines = [
        f"acceptance({doc['package']}): {doc['platform']} {doc['verdict']} @ {doc['commit'][:7]}",
        "",
        f"{pkg['title']}",
        f"Host: {h['machine']} — {h['os']}, {h['cpu']} ({h['cores']} threads), {h['rustc']}",
        "Criteria: " + ", ".join(f"{k}={v}" for k, v in sorted(counts.items())),
        "Suites: " + ", ".join(f"{sid} rc={s['rc']} ({s['tests']} tests, {s['seconds']}s)"
                               for sid, s in doc["suites"].items()),
    ]
    for title, pred in (
        ("FAILED", lambda r: r["status"] == "fail"),
        ("Findings (perf, machine-dependent)", lambda r: r["status"] == "finding"),
        ("Pending (ручная проверка или не задан файл-образец)", lambda r: r["status"] == "pending"),
    ):
        items = [(cid, r) for cid, r in res.items() if pred(r)]
        if items:
            lines += ["", f"{title}:"]
            for cid, r in items:
                extra = f" [known issue {r['known_issue']}]" if r.get("known_issue") else ""
                lines.append(f"  - {cid}{extra}")
                lines += [f"      {p}" for p in r.get("problems", [])]
    metrics = [(cid, m) for cid, r in res.items() for m in r.get("metrics", [])]
    if metrics:
        lines += ["", "Measurements:"]
        lines += [f"  - {cid}: {m}" for cid, m in metrics]
    return "\n".join(lines) + "\n"


# ----------------------------------------------------------------- status

def platform_state(pkg: dict, name: str, plat: str) -> tuple[str, str]:
    rp = results_path(name, plat)
    if not rp.exists():
        return "missing", "не прогонялось"
    doc = load_yaml(rp)
    commit = doc.get("commit", "")
    if subprocess.run(
        ["git", "cat-file", "-e", f"{commit}^{{commit}}"], cwd=ROOT, capture_output=True
    ).returncode != 0:
        return "stale", f"коммит {commit[:7]} не найден (git pull?)"
    changed = git("diff", "--name-only", commit, "HEAD", "--", *pkg["code_paths"])
    info = f"{doc.get('verdict')} @ {commit[:7]}, {doc.get('date', '')[:10]}, {doc.get('host', {}).get('machine', '?')}"
    if changed:
        n = len(changed.splitlines())
        return "stale", f"{info}; устарел: после прогона изменено {n} файл(ов) кода"
    return doc.get("verdict", "?"), info


def cmd_status(args) -> int:
    names = [args.name] if getattr(args, "name", None) else list_packages()
    if not names:
        print("Пакетов приёмки нет (docs/acceptance/*/criteria.yaml).")
        return 0
    all_ok = True
    icons = {"pass": "✅", "pass_with_known_issues": "✅", "missing": "⏳", "stale": "♻️",
             "incomplete": "⏸️", "needs_decision": "❓", "fail": "❌"}
    for name in names:
        pkg = load_yaml(package_dir(name) / "criteria.yaml")
        states = {p: platform_state(pkg, name, p) for p in pkg["platforms"]}
        accepted = all(s in ("pass", "pass_with_known_issues") for s, _ in states.values())
        all_ok &= accepted
        print(f"{name}: {'ПРИНЯТ' if accepted else 'не принят'} — {pkg['title']}")
        for p, (s, info) in states.items():
            print(f"  {icons.get(s, '?')} {p:8} {s:24} {info}")
        for p in pkg["platforms"]:
            rp = results_path(name, p)
            if not rp.exists():
                continue
            for cid, r in (load_yaml(rp).get("criteria") or {}).items():
                if r["status"] in ("fail", "finding", "pending"):
                    tag = f" [{r['known_issue']}]" if r.get("known_issue") else ""
                    print(f"      {p}: {r['status']:8} {cid}{tag}")
    return 0 if all_ok else 1


# ----------------------------------------------------------------- main

def _force_utf8_console() -> None:
    for stream in (sys.stdout, sys.stderr):
        if getattr(stream, "encoding", "").lower() != "utf-8":
            try:
                stream.reconfigure(encoding="utf-8")
            except (AttributeError, OSError):
                pass


def main(argv: list[str]) -> int:
    _force_utf8_console()
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("name")
    r.add_argument("--skip-manual", action="store_true", help="не задавать ручные вопросы (останутся pending)")
    r.add_argument("--no-commit", action="store_true", help="записать результат, но не коммитить")
    s = sub.add_parser("status")
    s.add_argument("name", nargs="?")
    sub.add_parser("check")
    args = ap.parse_args(argv)
    return {"run": cmd_run, "status": cmd_status, "check": cmd_check}[args.cmd](args)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
