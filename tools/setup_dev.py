#!/usr/bin/env python3
"""One-time per-machine/per-clone dev environment setup for APAP tooling.

Run this once after cloning (or on a new machine):

    python tools/setup_dev.py

It does three things, each idempotent:
  1. Installs the Python packages the tooling needs (tools/requirements.txt).
  2. Records the interpreter that just ran this script (sys.executable) in
     local git config (apap.pythonPath) so git hooks find the right Python
     even when `python` on PATH resolves to something else (e.g. the
     Windows Store stub) — this is what keeps hooks working across machines.
  3. Points git at the versioned hooks/ directory (core.hooksPath), so
     pre-commit/pre-push checks are active without copying files into
     .git/hooks by hand.

Nothing here touches AGENTS.md or any shared/committed config — everything
this script writes is local-only (git config without --global, not tracked).
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    print(f"$ {' '.join(cmd)}")
    return subprocess.run(cmd, cwd=ROOT, check=True, **kw)


def main() -> int:
    print(f"Using interpreter: {sys.executable}")

    run([sys.executable, "-m", "pip", "install", "-q", "-r", "tools/requirements.txt"])

    run(["git", "config", "apap.pythonPath", sys.executable])

    run(["git", "config", "core.hooksPath", "hooks"])

    hook = ROOT / "hooks" / "pre-commit"
    if hook.exists():
        try:
            hook.chmod(hook.stat().st_mode | 0o111)
        except OSError:
            pass  # not fatal on filesystems without exec bits (e.g. some Windows setups)

    print("\nDone. Verifying...")
    run([sys.executable, "tools/state_tool.py", "check"])
    print("\ntools/setup_dev.py: OK — hooks active, _STATE_ tooling verified.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
