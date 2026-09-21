"""Validation and canonicalization for the Lease Fence request shape."""

from __future__ import annotations

import hashlib
import json
import re
from typing import Any


ID_PATTERN = re.compile(r"^[A-Za-z0-9._-]+$")
TOP_LEVEL_FIELDS = {"pipeline_id", "jobs"}
JOB_FIELDS = {"job_id", "kind", "payload", "depends_on", "input_refs", "collect"}


class ValidationError(ValueError):
    """A client supplied a malformed or semantically invalid pipeline."""


def _require_id(value: Any, label: str) -> str:
    if not isinstance(value, str):
        raise ValidationError(f"{label} must be a string")
    value = value.strip()
    if not value or ID_PATTERN.fullmatch(value) is None:
        raise ValidationError(f"{label} must contain only ASCII letters, digits, '.', '_' or '-'")
    return value


def _require_object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValidationError(f"{label} must be an object")
    return value


def _require_field(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip() or "." in value:
        raise ValidationError(f"{label} must be a non-empty top-level field")
    return value.strip()


def normalize_pipeline(document: Any) -> dict[str, Any]:
    """Validate and return a deterministic, JSON-compatible pipeline document."""

    if not isinstance(document, dict) or set(document) != TOP_LEVEL_FIELDS:
        raise ValidationError("pipeline must contain exactly pipeline_id and jobs")
    pipeline_id = _require_id(document["pipeline_id"], "pipeline_id")
    jobs_value = document["jobs"]
    if not isinstance(jobs_value, list) or not jobs_value:
        raise ValidationError("jobs must be a non-empty list")

    jobs: list[dict[str, Any]] = []
    job_ids: set[str] = set()
    for index, raw_job in enumerate(jobs_value):
        if not isinstance(raw_job, dict) or set(raw_job) != JOB_FIELDS:
            raise ValidationError(f"job {index} must contain exactly the six documented fields")
        job_id = _require_id(raw_job["job_id"], f"job {index} job_id")
        if job_id in job_ids:
            raise ValidationError(f"duplicate job_id: {job_id}")
        job_ids.add(job_id)

        kind = raw_job["kind"]
        if not isinstance(kind, str) or not kind.strip():
            raise ValidationError(f"job {job_id} kind must be a non-empty string")
        kind = kind.strip()
        payload = _require_object(raw_job["payload"], f"job {job_id} payload")

        dependencies = raw_job["depends_on"]
        if not isinstance(dependencies, list):
            raise ValidationError(f"job {job_id} depends_on must be a list")
        normalized_dependencies: list[str] = []
        for dependency in dependencies:
            normalized = _require_id(dependency, f"job {job_id} dependency")
            if normalized in normalized_dependencies:
                raise ValidationError(f"job {job_id} has duplicate dependency: {normalized}")
            normalized_dependencies.append(normalized)

        input_refs = _require_object(raw_job["input_refs"], f"job {job_id} input_refs")
        normalized_refs: dict[str, dict[str, str]] = {}
        for input_name, raw_ref in input_refs.items():
            normalized_name = _require_id(input_name, f"job {job_id} input name")
            if not isinstance(raw_ref, dict) or set(raw_ref) != {"job_id", "field"}:
                raise ValidationError(f"job {job_id} input reference {normalized_name} is malformed")
            source_job = _require_id(raw_ref["job_id"], f"job {job_id} input source")
            field = _require_field(raw_ref["field"], f"job {job_id} input {normalized_name}")
            normalized_refs[normalized_name] = {"job_id": source_job, "field": field}

        raw_collect = raw_job["collect"]
        if raw_collect is None:
            collect = None
        else:
            if not isinstance(raw_collect, dict) or set(raw_collect) != {"field", "as"}:
                raise ValidationError(f"job {job_id} collect must be null or contain exactly field and as")
            collect = {
                "field": _require_field(raw_collect["field"], f"job {job_id} collect field"),
                "as": _require_id(raw_collect["as"], f"job {job_id} collect name"),
            }
        if kind == "barrier":
            if collect is None or len(normalized_dependencies) < 2:
                raise ValidationError("barrier jobs require collect and at least two dependencies")
        elif collect is not None:
            raise ValidationError("only barrier jobs may set collect")

        jobs.append(
            {
                "job_id": job_id,
                "kind": kind,
                "payload": payload,
                "depends_on": normalized_dependencies,
                "input_refs": normalized_refs,
                "collect": collect,
            }
        )

    for job in jobs:
        for dependency in job["depends_on"]:
            if dependency not in job_ids:
                raise ValidationError(f"job {job['job_id']} depends on missing job: {dependency}")
        for input_name, reference in job["input_refs"].items():
            source_job = reference["job_id"]
            if source_job not in job_ids:
                raise ValidationError(f"job {job['job_id']} input {input_name} references missing job: {source_job}")
            if source_job not in job["depends_on"]:
                raise ValidationError(
                    f"job {job['job_id']} input {input_name} references an undeclared dependency: {source_job}"
                )

    _check_acyclic(jobs)
    return {"pipeline_id": pipeline_id, "jobs": jobs}


def _check_acyclic(jobs: list[dict[str, Any]]) -> None:
    dependencies = {job["job_id"]: set(job["depends_on"]) for job in jobs}
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


def canonical_jobs(jobs: list[dict[str, Any]]) -> str:
    return json.dumps(jobs, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def content_hash(document: dict[str, Any]) -> str:
    return hashlib.sha256(canonical_jobs(document["jobs"]).encode("utf-8")).hexdigest()
