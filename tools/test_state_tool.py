#!/usr/bin/env python3
"""Тесts for tools/state_tool.py.

WF1.0-B11: cmd_close_bug консервирует _STATE_.yaml текстовыми regex-
подстановками (closing.md §4); регрессионный тест воспроизводит блочный
скаляр `dod: >` (и блочный скаляр внутри steps[].description, как в
реальных сессиях) и проверяет, что результат — валидный YAML, а не
обрывок с висящими строками-продолжением под `dod: null`
(`yaml.scanner.ScannerError: mapping values are not allowed here`).

Run:
    python tools/test_state_tool.py
    python -m unittest tools/test_state_tool.py
"""
from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import state_tool  # noqa: E402

STATE_YAML_BLOCK_DOD = """\
status: in_progress
task:
  roadmap_id: WF1.0-B11
  title: "test bug"
whitelist:
  - tools/state_tool.py
dod: >
  Многострочный критерий готовности close-bug,
  занимает несколько строк отступа (блочный скаляр).
steps:
  - id: 1
    description: >
      Многострочное описание шага (блочный скаляр),
      тоже должно консервироваться без потерь.
    done: true
current_step: 1
next_action: "закрытие по closing.md"
fail_counter: 0
fail_counter_max: 3
failed_attempts: []
plan:
  title: "test plan"
  items:
    - id: 1
      description: "item"
      status: done
"""

ROADMAP_TEMPLATE = (
    "# ROADMAP\n\n"
    "| ID | Задача | Приоритет | Статус | ТЗ |\n"
    "|---|---|---|---|---|\n"
    "| WF1.0-B11 | test bug | 🟢 Средний | 🔄 выполняется | [§4](x) |\n"
)

ARCHIVE_TEMPLATE = "# Архив багов\n"


class CloseBugBlockScalarTest(unittest.TestCase):
    """Воспроизводит WF1.0-B11 на изолированных временных файлах — реальные
    _STATE_.yaml/ROADMAP.md/archive_bugs.md репозитория не трогаются."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        tmp_path = Path(self.tmp.name)

        self.state_yaml = tmp_path / "_STATE_.yaml"
        self.state_md = tmp_path / "_STATE_.md"
        self.roadmap_md = tmp_path / "ROADMAP.md"
        self.archive_md = tmp_path / "archive_bugs.md"

        self.state_yaml.write_text(STATE_YAML_BLOCK_DOD, encoding="utf-8")
        self.roadmap_md.write_text(ROADMAP_TEMPLATE, encoding="utf-8")
        self.archive_md.write_text(ARCHIVE_TEMPLATE, encoding="utf-8")

        for attr, value in (
            ("STATE_YAML", self.state_yaml),
            ("STATE_MD", self.state_md),
            ("ROADMAP_MD", self.roadmap_md),
            ("ARCHIVE_BUGS_MD", self.archive_md),
        ):
            patcher = mock.patch.object(state_tool, attr, value)
            patcher.start()
            self.addCleanup(patcher.stop)

        branch_patcher = mock.patch.object(state_tool, "current_branch", lambda: "main")
        branch_patcher.start()
        self.addCleanup(branch_patcher.stop)

    def test_close_bug_handles_block_scalar_dod_and_multiline_step(self) -> None:
        rc = state_tool.cmd_close_bug(["WF1.0-B11", "deadbee"])
        self.assertEqual(rc, 0)

        new_text = self.state_yaml.read_text(encoding="utf-8")
        # До фикса здесь падал yaml.scanner.ScannerError — висящие строки
        # блочного скаляра под "dod: null" как невалидный YAML верхнего
        # уровня (WF1.0-B11).
        data = state_tool.load_yaml_text(new_text)

        self.assertEqual(data["status"], "done")
        self.assertIsNone(data["dod"])
        self.assertEqual(data["whitelist"], [])
        self.assertEqual(data["steps"], [])
        self.assertIsNone(data["current_step"])
        self.assertIsNone(data["next_action"])
        self.assertEqual(data["fail_counter"], 0)
        self.assertEqual(data["failed_attempts"], [])
        self.assertEqual(data["task"], {"roadmap_id": None, "title": None})

        # "plan:" и всё под ним — байт-в-байт нетронуто (closing.md §4),
        # включая его собственные многострочные поля.
        self.assertIn('title: "test plan"', new_text)
        self.assertNotIn("занимает несколько строк отступа", new_text)
        self.assertEqual(data["plan"]["title"], "test plan")

        archive_text = self.archive_md.read_text(encoding="utf-8")
        self.assertIn("✅ сделано в deadbee (ветка main)", archive_text)

        roadmap_text = self.roadmap_md.read_text(encoding="utf-8")
        self.assertNotIn("WF1.0-B11", roadmap_text)

    def test_resulting_state_yaml_passes_render(self) -> None:
        rc = state_tool.cmd_close_bug(["WF1.0-B11", "deadbee"])
        self.assertEqual(rc, 0)
        data = state_tool.load_yaml_text(self.state_yaml.read_text(encoding="utf-8"))
        errors = state_tool.validate_schema(data) + state_tool.validate_business_rules(data)
        self.assertEqual(errors, [])


if __name__ == "__main__":
    unittest.main()
