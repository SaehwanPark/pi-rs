from __future__ import annotations

import re
from typing import Any


ID_RE = re.compile(r"^[A-Za-z0-9._-]+$")
JOB_FIELDS = {"job_id", "kind", "payload", "depends_on"}


class ValidationError(ValueError):
    """A request does not satisfy the public batch contract."""


def _text(value: Any, label: str) -> str:
    if not isinstance(value, str):
        raise ValidationError(f"{label} must be a string")
    value = value.strip()
    if not value or not ID_RE.fullmatch(value):
        raise ValidationError(f"{label} must be a non-empty ASCII identifier")
    return value


def _check_acyclic(jobs: list[dict[str, Any]]) -> None:
    dependencies = {job["job_id"]: job["depends_on"] for job in jobs}
    visiting: set[str] = set()
    visited: set[str] = set()

    def visit(job_id: str) -> None:
        if job_id in visiting:
            raise ValidationError("job dependencies must be acyclic")
        if job_id in visited:
            return
        visiting.add(job_id)
        for dependency in dependencies[job_id]:
            visit(dependency)
        visiting.remove(job_id)
        visited.add(job_id)

    for job_id in dependencies:
        visit(job_id)


def validate_batch(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"batch_id", "jobs"}:
        raise ValidationError("batch must contain only batch_id and jobs")
    batch_id = _text(value.get("batch_id"), "batch_id")
    raw_jobs = value.get("jobs")
    if not isinstance(raw_jobs, list) or not raw_jobs:
        raise ValidationError("jobs must be a non-empty list")

    jobs: list[dict[str, Any]] = []
    job_ids: set[str] = set()
    for raw_job in raw_jobs:
        if not isinstance(raw_job, dict) or set(raw_job) != JOB_FIELDS:
            raise ValidationError("each job must contain exactly the documented fields")
        job_id = _text(raw_job.get("job_id"), "job_id")
        if job_id in job_ids:
            raise ValidationError(f"duplicate job_id: {job_id}")
        job_ids.add(job_id)
        kind = raw_job.get("kind")
        if not isinstance(kind, str) or not kind.strip():
            raise ValidationError("kind must be a non-empty string")
        payload = raw_job.get("payload")
        if not isinstance(payload, dict):
            raise ValidationError("payload must be an object")
        raw_dependencies = raw_job.get("depends_on")
        if not isinstance(raw_dependencies, list):
            raise ValidationError("depends_on must be a list")
        dependencies: list[str] = []
        for raw_dependency in raw_dependencies:
            dependency = _text(raw_dependency, "dependency")
            if dependency in dependencies:
                raise ValidationError(f"duplicate dependency: {dependency}")
            dependencies.append(dependency)
        jobs.append(
            {
                "job_id": job_id,
                "kind": kind.strip(),
                "payload": payload,
                "depends_on": dependencies,
            }
        )

    for job in jobs:
        missing = [dependency for dependency in job["depends_on"] if dependency not in job_ids]
        if missing:
            raise ValidationError(f"unknown dependency: {missing[0]}")
    _check_acyclic(jobs)
    return {"batch_id": batch_id, "jobs": jobs}
