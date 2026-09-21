"""Independent NDJSON sink used only by the Batch Relay oracle."""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--log", required=True)
    parser.add_argument("--retry-job")
    parser.add_argument("--retry-marker")
    parser.add_argument("--fail-job")
    parser.add_argument("--block-file")
    parser.add_argument("--release-file")
    args = parser.parse_args()

    log_path = Path(args.log)
    log_path.parent.mkdir(parents=True, exist_ok=True)
    release_path = Path(args.release_file) if args.release_file else None

    for line in __import__("sys").stdin:
        job = json.loads(line)
        job_id = job["job_id"]
        with log_path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(job, sort_keys=True) + "\n")

        if args.block_file:
            Path(args.block_file).write_text("claimed\n", encoding="utf-8")
            while release_path is None or not release_path.exists():
                time.sleep(0.02)

        if args.fail_job == job_id:
            response = {"job_id": job_id, "ok": False, "retryable": False, "error": "permanent sink rejection"}
        elif args.retry_job == job_id and args.retry_marker:
            marker = Path(args.retry_marker)
            if not marker.exists():
                marker.write_text("failed once\n", encoding="utf-8")
                response = {"job_id": job_id, "ok": False, "retryable": True, "error": "temporary sink rejection"}
            else:
                response = {"job_id": job_id, "ok": True}
        else:
            response = {"job_id": job_id, "ok": True}

        print(json.dumps(response, separators=(",", ":")), flush=True)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
