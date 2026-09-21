from __future__ import annotations

import argparse
import json
import os
import time
from pathlib import Path
import sys


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--log", required=True)
    parser.add_argument("--label", default="sink")
    parser.add_argument("--receipts", required=True)
    parser.add_argument("--block-before-apply-job")
    parser.add_argument("--block-after-apply-job")
    parser.add_argument("--claimed-file")
    parser.add_argument("--applied-file")
    parser.add_argument("--pid-file")
    parser.add_argument("--release-file")
    parser.add_argument("--retry-job", action="append", default=[])
    parser.add_argument("--fail-job", action="append", default=[])
    parser.add_argument("--lose-ack-job", action="append", default=[])
    return parser.parse_args()


def append_log(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")


def wait_for_release(path: Path | None) -> None:
    while path is None or not path.exists():
        time.sleep(0.05)


def load_receipts(path: Path) -> dict:
    if not path.exists():
        return {}
    value = json.loads(path.read_text(encoding="utf-8"))
    return value if isinstance(value, dict) else {}


def save_receipts(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, sort_keys=True), encoding="utf-8")
    os.replace(temporary, path)


def make_output(value: dict, label: str) -> dict:
    pipeline_id = value["pipeline_id"]
    job_id = value["job_id"]
    output = {
        "artifact_id": f"artifact-{pipeline_id}-{job_id}",
        "private": f"private-{job_id}",
        "kind_seen": value["kind"],
        "worker_label": label,
    }
    if value["kind"] == "barrier":
        fan_in = value.get("fan_in")
        if not isinstance(fan_in, dict) or not isinstance(fan_in.get("items"), list):
            raise ValueError("barrier fan-in missing")
        output["joined"] = [item["value"] for item in fan_in["items"]]
        output["joined_ids"] = [item["job_id"] for item in fan_in["items"]]
    elif value["kind"] == "publish":
        output["used"] = value.get("inputs", {}).get("joined")
    return output


def main() -> int:
    args = parse_args()
    if args.pid_file:
        Path(args.pid_file).write_text(str(os.getpid()) + "\n", encoding="utf-8")
    line = sys.stdin.readline()
    if not line:
        return 2
    try:
        value = json.loads(line)
    except json.JSONDecodeError:
        return 2
    if not isinstance(value, dict):
        return 2
    job_id = value.get("job_id")
    delivery_key = value.get("delivery_key")
    if not isinstance(job_id, str) or not isinstance(delivery_key, str):
        return 2

    release = Path(args.release_file) if args.release_file else None
    if job_id == args.block_before_apply_job:
        if args.claimed_file:
            Path(args.claimed_file).write_text("claimed\n", encoding="utf-8")
        wait_for_release(release)

    if job_id in args.retry_job:
        append_log(Path(args.log), {
            "applied": False,
            "delivery_key": delivery_key,
            "fan_in": value.get("fan_in"),
            "job_id": job_id,
            "label": args.label,
            "inputs": value.get("inputs"),
            "replayed": False,
        })
        print(json.dumps({
            "delivery_key": delivery_key,
            "job_id": job_id,
            "ok": False,
            "retryable": True,
            "error": "retry requested",
        }))
        return 0
    if job_id in args.fail_job:
        append_log(Path(args.log), {
            "applied": False,
            "delivery_key": delivery_key,
            "fan_in": value.get("fan_in"),
            "job_id": job_id,
            "label": args.label,
            "inputs": value.get("inputs"),
            "replayed": False,
        })
        print(json.dumps({
            "delivery_key": delivery_key,
            "job_id": job_id,
            "ok": False,
            "retryable": False,
            "error": "terminal requested",
        }))
        return 0

    receipts_path = Path(args.receipts)
    receipts = load_receipts(receipts_path)
    record = receipts.get(delivery_key)
    replayed = isinstance(record, dict)
    applied = False
    if not replayed:
        try:
            output = make_output(value, args.label)
        except (KeyError, TypeError, ValueError):
            return 2
        record = {
            "output": output,
            "receipt_id": "receipt-" + delivery_key.replace(":", "-"),
        }
        receipts[delivery_key] = record
        save_receipts(receipts_path, receipts)
        applied = True
        if args.applied_file:
            Path(args.applied_file).write_text("applied\n", encoding="utf-8")

    append_log(Path(args.log), {
        "applied": applied,
        "delivery_key": delivery_key,
        "fan_in": value.get("fan_in"),
        "job_id": job_id,
        "label": args.label,
        "inputs": value.get("inputs"),
        "replayed": replayed,
    })

    if job_id == args.block_after_apply_job:
        wait_for_release(release)
        return 3
    if job_id in args.lose_ack_job:
        return 3

    print(json.dumps({
        "delivery_key": delivery_key,
        "job_id": job_id,
        "ok": True,
        "output": record["output"],
        "receipt_id": record["receipt_id"],
    }, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
