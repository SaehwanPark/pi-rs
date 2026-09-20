from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]


class CliTests(unittest.TestCase):
    def test_help_commands_are_available(self) -> None:
        for arguments in (["--help"], ["serve", "--help"], ["worker", "--help"]):
            result = subprocess.run(
                [sys.executable, "-m", "webhookinbox", *arguments],
                cwd=PROJECT_ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(result.stdout.strip())


if __name__ == "__main__":
    unittest.main()
