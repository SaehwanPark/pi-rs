"""Independent sink that records a request and waits for release."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
import time


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--marker", required=True)
    parser.add_argument("--release", required=True)
    args = parser.parse_args()

    line = sys.stdin.readline()
    if not line:
        return 2
    try:
        request = json.loads(line)
    except json.JSONDecodeError:
        return 3
    Path(args.marker).write_text(
        json.dumps(request, sort_keys=True, separators=(",", ":")), encoding="utf-8"
    )
    deadline = time.monotonic() + 10.0
    while not Path(args.release).exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
