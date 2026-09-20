# Next black-box case: Reading Queue HTTP service

Status: draft  
Date: 2026-09-20  
Test operator: Codex acting as a new `rupi` user  
Model target: local llama.cpp, `qwen3.8-flash-next`

## T and why it is next

Build **Read Queue**, a dependency-free Python 3 JSON HTTP service for a small
personal reading queue. It stores records in SQLite, serves a documented API,
and includes unit plus subprocess integration tests.

The completed Test Ledger case already exercises a local CLI, file-backed JSON
CRUD, separate Python processes, validation, and independent acceptance checks.
Read Queue is the next difficulty step because it adds a long-lived process,
HTTP request/response boundaries, relational persistence, restart recovery, and
integration-test orchestration while remaining one small standard-library
project. It is not another task-list CLI or a one-file demo.

## Bounded specification

Owned paths are `docs/cases/2026-09-20-reading-queue/` only. The project root is
`project/`; the independent acceptance oracle is `acceptance/`.

The model must build `project/readqueue` and a runnable `README.md` from
`project/SPEC.md`. The service must provide:

- `python -m readqueue serve --db PATH --host HOST --port PORT`;
- `GET /healthz`, `GET /items`, `GET /items/<id>`;
- `POST /items` to create a queued item with a title, URL, and optional tags;
- `PATCH /items/<id>` to change the title, URL, tags, or status;
- `DELETE /items/<id>`;
- deterministic JSON responses, useful 4xx errors, and SQLite persistence.

## Observable acceptance criteria

1. `python -m unittest discover -s tests -p "test_*.py" -v` passes from
   `project/`.
2. The independent acceptance oracle starts the server in a fresh process,
   waits for `/healthz`, creates and lists items, filters by status/tag, reads,
   patches, deletes, and checks structured invalid-input/404 responses.
3. The oracle stops and restarts the server against the same database and sees
   the surviving item with its updated state.
4. Invalid requests do not add, partially update, or delete records.
5. The project uses only the Python standard library and documents start-up,
   routes, persistence, and test commands in `README.md`.
6. A live `rupi run` performs the implementation and verification work in the
   confined project workspace; the final project is then checked independently.

## Non-goals

- authentication, authorization, HTTPS, rate limiting, or deployment packaging;
- concurrent-write guarantees beyond the standard library server/database use;
- migrations, multiple database backends, or third-party dependencies;
- a browser UI, CLI client, background jobs, or external service integration;
- changes to rupi source, tests, canonical architecture, or roadmap status.

## Evidence and stopgates

Record exact prompts, command lines, exit codes, elapsed times, stdout/stderr,
session ids, trace/replay results, independent test output, and every material
friction in `OBSERVATIONS.md`. A request-budget exhaustion, provider failure,
or recovery interruption is evidence even if a later run succeeds. Stop only on
a concrete environment or runtime gate after bounded recovery attempts; do not
claim project acceptance from model prose alone.

