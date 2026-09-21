"""Independent line-protocol sink used only by the Event Outbox oracle."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--log", required=True)
    parser.add_argument("--fail-event")
    args = parser.parse_args()
    log_path = Path(args.log)
    log_path.parent.mkdir(parents=True, exist_ok=True)

    for line in sys.stdin:
        message = json.loads(line)
        with log_path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(message, sort_keys=True) + "\n")
        event_id = message.get("event_id")
        if event_id == args.fail_event:
            response = {"event_id": event_id, "ok": False, "error": "planned sink rejection"}
        else:
            response = {"event_id": event_id, "ok": True}
        print(json.dumps(response, sort_keys=True), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
