"""Independent successful sink used by the Webhook Inbox oracle."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--log", required=True)
    parser.add_argument("--tag", required=True)
    args = parser.parse_args()

    line = sys.stdin.readline()
    if not line:
        return 2
    try:
        request = json.loads(line)
    except json.JSONDecodeError:
        return 3
    if not isinstance(request, dict) or not isinstance(request.get("delivery_id"), str):
        return 4

    record = {"tag": args.tag, "request": request}
    log_path = Path(args.log)
    with log_path.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
    response = {"delivery_id": request["delivery_id"], "ok": True}
    sys.stdout.write(json.dumps(response, sort_keys=True, separators=(",", ":")) + "\n")
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
