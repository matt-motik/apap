#!/usr/bin/env python3
"""Запрещённые конструкции в коде аудио-колбэка (AM1.0 §7.7; ТЗ-98, ТЗ-102, И-Р17).

В файлах `src/audio/render/**` (колбэк реального времени) не должно быть
блокировок, логирования, ввода-вывода и аллокаций:
`Mutex`, `RwLock`, `Condvar`, `log::`, `tracing::`, `println!`, `eprintln!`,
`std::fs`, `std::io`, `Box::new`, `Vec::`, `vec!`, `String`.

Комментарии не проверяются. Тестовый код не проверяется: всё, начиная со
строки `#[cfg(test)]`, пропускается (тестовый модуль — в конце файла).

Использование:
    python tools/check_rt_imports.py            # src/audio/render/**
    python tools/check_rt_imports.py PATH...    # заданные файлы или каталоги
Код выхода: 0 — нарушений нет, 1 — есть (список в stdout).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_ROOTS = [REPO / "src" / "audio" / "render"]

FORBIDDEN: list[tuple[str, re.Pattern[str]]] = [
    ("Mutex", re.compile(r"\bMutex\b")),
    ("RwLock", re.compile(r"\bRwLock\b")),
    ("Condvar", re.compile(r"\bCondvar\b")),
    ("log::", re.compile(r"\blog::")),
    ("tracing::", re.compile(r"\btracing::")),
    ("println!", re.compile(r"\bprintln!")),
    ("eprintln!", re.compile(r"\beprintln!")),
    ("std::fs", re.compile(r"\bstd::fs\b")),
    ("std::io", re.compile(r"\bstd::io\b")),
    ("Box::new", re.compile(r"\bBox::new\b")),
    ("Vec::", re.compile(r"\bVec::")),
    ("vec!", re.compile(r"\bvec!")),
    ("String", re.compile(r"\bString\b")),
]

TEST_START = re.compile(r"^\s*#\[cfg\(test\)\]")


def strip_comments(text: str) -> list[str]:
    """Строки кода без комментариев `//…` и `/* … */` (строковые литералы
    сохраняются: запрещённое имя внутри литерала — тоже повод посмотреть)."""
    out: list[str] = []
    in_block = False
    for line in text.splitlines():
        code = ""
        i = 0
        while i < len(line):
            if in_block:
                end = line.find("*/", i)
                if end < 0:
                    i = len(line)
                else:
                    in_block = False
                    i = end + 2
                continue
            if line.startswith("//", i):
                break
            if line.startswith("/*", i):
                in_block = True
                i += 2
                continue
            code += line[i]
            i += 1
        out.append(code)
    return out


def check_file(path: Path) -> list[str]:
    errors: list[str] = []
    raw = path.read_text(encoding="utf-8").splitlines()
    for lineno, (code, original) in enumerate(zip(strip_comments("\n".join(raw)), raw), start=1):
        if TEST_START.match(original):
            break
        for name, pattern in FORBIDDEN:
            if pattern.search(code):
                errors.append(f"{path}:{lineno}: запрещено `{name}` в коде колбэка: {original.strip()}")
    return errors


def rust_files(roots: list[Path]) -> list[Path]:
    files: list[Path] = []
    for root in roots:
        if root.is_file():
            files.append(root)
        elif root.is_dir():
            files.extend(sorted(root.rglob("*.rs")))
    return files


def main(argv: list[str]) -> int:
    roots = [Path(a).resolve() for a in argv] if argv else DEFAULT_ROOTS
    files = rust_files(roots)
    errors = [e for f in files for e in check_file(f)]
    for e in errors:
        print(e)
    if errors:
        print(f"FAIL — {len(errors)} нарушений в коде колбэка (AM1.0 §7.7).")
        return 1
    print(f"OK — проверено файлов колбэка: {len(files)}.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
