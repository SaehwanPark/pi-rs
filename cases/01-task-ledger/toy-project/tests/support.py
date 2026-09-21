"""Shared test fixtures: run the CLI in a temporary directory, clean environment.

Kept out of ``test_*.py`` so unittest discovery does not collect it as a test
module while every test file can still reuse the same isolation rules.
"""

from __future__ import annotations

import io
import json
import os
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path
from typing import Any

from tasklog.cli import run

STATE_NAME = ".tasklog.json"


class TempDirCase(unittest.TestCase):
    """Base class giving each test an empty directory and no leaked env vars.

    ``TASKLOG_PATH`` is stripped from the environment because it outranks the
    temporary base directory: inheriting it from a developer's shell would
    silently point every test at the same ledger.
    """

    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.base = Path(temp.name)
        self.state = self.base / STATE_NAME

        env = {k: v for k, v in os.environ.items() if k.upper() != "TASKLOG_PATH"}
        patcher = unittest.mock.patch.dict(os.environ, env, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    # -- running the CLI --------------------------------------------------

    def run_cli(self, *argv: str) -> tuple[int, str, str]:
        """Return ``(exit_code, stdout, stderr)`` for one CLI invocation.

        ``sys.stdout``/``sys.stderr`` are redirected as well as the streams
        handed to ``run`` so argparse's own help and usage output is captured.
        ``SystemExit`` is converted back to its code: argparse reports usage
        errors and ``--help`` by exiting, and the tests assert on the code.
        """
        out, err = io.StringIO(), io.StringIO()
        with unittest.mock.patch.object(sys, "stdout", out), (
            unittest.mock.patch.object(sys, "stderr", err)
        ):
            try:
                code = run(list(argv), out, err, base=self.base)
            except SystemExit as exc:
                code = 0 if exc.code is None else exc.code
        return int(code), out.getvalue(), err.getvalue()

    def must_run(self, *argv: str) -> str:
        """Run ``argv`` expecting success and return its stdout."""
        code, out, err = self.run_cli(*argv)
        self.assertEqual(code, 0, msg=f"argv={argv!r} stderr={err!r}")
        return out

    # -- inspecting the state file ---------------------------------------

    def state_bytes(self) -> bytes:
        return self.state.read_bytes()

    def state_document(self) -> dict[str, Any]:
        return json.loads(self.state.read_text(encoding="utf-8"))

    def write_state(self, contents: str | dict[str, Any]) -> None:
        text = (
            contents
            if isinstance(contents, str)
            else json.dumps(contents, indent=2, sort_keys=True)
        )
        self.state.write_text(text, encoding="utf-8")

    def temp_files(self) -> list[str]:
        """Names of leftover temporary files, which must never accumulate."""
        return sorted(p.name for p in self.base.iterdir() if p.suffix == ".tmp")
