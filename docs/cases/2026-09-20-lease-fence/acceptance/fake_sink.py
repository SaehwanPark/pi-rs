from __future__ import annotations

import argparse
import json
import time
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--log", required=True)
    parser.add_argument("--label", default="sink")
    parser.add_argument("--block-file")
    parser.add_argument("--block-job")
    parser.add_argument("--release-file")
    parser.add_argument("--retry-job", action="append", default=[])
    parser.add_argument("--fail-job", action="append", default=[])
    parser.add_argument("--omit-field", action="append", default=[])
    return parser.parse_args()


def append_log(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")


def main() -> int:
    args = parse_args()
    line = __import__("sys").stdin.readline()
    if not line:
        return 2
    try:
        value = json.loads(line)
    except json.JSONDecodeError:
        return 2
    if not isinstance(value, dict) or not isinstance(value.get("job_id"), str):
        return 2
    append_log(Path(args.log), value)
    job_id = value["job_id"]

    if args.block_file and (args.block_job is None or args.block_job == job_id):
        Path(args.block_file).write_text("claimed\n", encoding="utf-8")
        release = Path(args.release_file) if args.release_file else None
        while release is None or not release.exists():
            time.sleep(0.05)

    if job_id in args.retry_job:
        print(json.dumps({"job_id": job_id, "ok": False, "retryable": True, "error": "retry requested"}))
        return 0
    if job_id in args.fail_job:
        print(json.dumps({"job_id": job_id, "ok": False, "retryable": False, "error": "terminal requested"}))
        return 0

    pipeline_id = value["pipeline_id"]
    kind = value["kind"]
    output = {
        "artifact_id": f"artifact-{pipeline_id}-{job_id}",
        "private": f"private-{job_id}",
        "kind_seen": kind,
        "worker_label": args.label,
    }
    if job_id in args.omit_field:
        output.pop("artifact_id")
    if kind == "barrier":
        fan_in = value.get("fan_in")
        if not isinstance(fan_in, dict) or not isinstance(fan_in.get("items"), list):
            return 2
        output["joined"] = [item["value"] for item in fan_in["items"]]
        output["joined_ids"] = [item["job_id"] for item in fan_in["items"]]
    elif kind == "publish":
        output["used"] = value.get("inputs", {}).get("joined")
    print(json.dumps({"job_id": job_id, "ok": True, "output": output}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
