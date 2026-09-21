# rupi Benchmark & Evaluation Cases

This directory contains ten progressive, dependency-free reference projects (the "ladder") used
to test, benchmark, and evaluate `rupi` across realistic software engineering tasks.

Every project is implemented using only the Python 3 standard library and local persistence
(JSON files or SQLite). Each project includes a complete specification ([`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/01-task-ledger/toy-project/SPEC.md)),
runnable commands, unit tests, and an independent fresh-process acceptance oracle that verifies
behavior without importing project code.

## The Case Ladder

The ladder progresses systematically from a basic command-line tool up to a distributed,
fault-tolerant pipeline service with cryptographic audit logging.

| # | Directory | Package | Architecture & Scope | Key Reliability / Contract Increment |
| :- | :--- | :--- | :--- | :--- |
| 01 | [`01-task-ledger`](file:///C:/Users/saehwan/repos/rupi/cases/01-task-ledger) | [`tasklog`](file:///C:/Users/saehwan/repos/rupi/cases/01-task-ledger/toy-project/tasklog) | Single-process CLI | File-backed JSON persistence, atomic writes, stable IDs |
| 02 | [`02-reading-queue`](file:///C:/Users/saehwan/repos/rupi/cases/02-reading-queue) | [`readqueue`](file:///C:/Users/saehwan/repos/rupi/cases/02-reading-queue/project/readqueue) | Long-lived HTTP service | Relational SQLite storage, CRUD validation, restart recovery |
| 03 | [`03-event-outbox`](file:///C:/Users/saehwan/repos/rupi/cases/03-event-outbox) | [`outbox`](file:///C:/Users/saehwan/repos/rupi/cases/03-event-outbox/project/outbox) | HTTP service + separate worker | Transactional outbox, client idempotency keys, NDJSON sink IPC |
| 04 | [`04-webhook-inbox`](file:///C:/Users/saehwan/repos/rupi/cases/04-webhook-inbox) | [`webhookinbox`](file:///C:/Users/saehwan/repos/rupi/cases/04-webhook-inbox/project/webhookinbox) | Ingestion service + worker | HMAC-SHA256 signature auth, leased delivery, crash reclaim |
| 05 | [`05-batch-relay`](file:///C:/Users/saehwan/repos/rupi/cases/05-batch-relay) | [`batchrelay`](file:///C:/Users/saehwan/repos/rupi/cases/05-batch-relay/project/batchrelay) | DAG job scheduler + worker | Atomic DAG admission, topological order, terminal failure cascades |
| 06 | [`06-artifact-pipeline`](file:///C:/Users/saehwan/repos/rupi/cases/06-artifact-pipeline) | [`artifactpipe`](file:///C:/Users/saehwan/repos/rupi/cases/06-artifact-pipeline/project/artifactpipe) | Data-flow DAG engine + worker | Structured JSON output persistence, declared scalar input propagation |
| 07 | [`07-lease-cascade`](file:///C:/Users/saehwan/repos/rupi/cases/07-lease-cascade) | [`leasecascade`](file:///C:/Users/saehwan/repos/rupi/cases/07-lease-cascade/project/leasecascade) | Barrier DAG engine + worker | Fan-out/fan-in barrier aggregation, ordered field collections |
| 08 | [`08-lease-fence`](file:///C:/Users/saehwan/repos/rupi/cases/08-lease-fence) | [`leasefence`](file:///C:/Users/saehwan/repos/rupi/cases/08-lease-fence/project/leasefence) | Distributed reliability worker | Optimistic claim fencing tokens, stale/zombie worker rejection |
| 09 | [`09-lease-receipt`](file:///C:/Users/saehwan/repos/rupi/cases/09-lease-receipt) | [`leasereceipt`](file:///C:/Users/saehwan/repos/rupi/cases/09-lease-receipt/project/leasereceipt) | Idempotent sink worker | Stable delivery keys, durable sink receipts, lost-ACK recovery |
| 10 | [`10-receipt-ledger`](file:///C:/Users/saehwan/repos/rupi/cases/10-receipt-ledger) | [`receiptledger`](file:///C:/Users/saehwan/repos/rupi/cases/10-receipt-ledger/project/receiptledger) | Audited capstone pipeline | Append-only SHA-256 tamper-evident audit chain, audit CLI tools |

---

## Case Summaries

### 01. Task Ledger

- **Directory**: [`cases/01-task-ledger/toy-project`](file:///C:/Users/saehwan/repos/rupi/cases/01-task-ledger/toy-project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/01-task-ledger/toy-project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/01-task-ledger/toy-project/README.md)
- **Acceptance Oracle**: [`acceptance/test_tasklog_subprocess.py`](file:///C:/Users/saehwan/repos/rupi/cases/01-task-ledger/acceptance/test_tasklog_subprocess.py)

**What it is & what to build**:
Build [`tasklog`](file:///C:/Users/saehwan/repos/rupi/cases/01-task-ledger/toy-project/tasklog), a dependency-free Python 3 CLI task management utility. The tool maintains tasks in a local `.tasklog.json` state file.

- Implement CLI commands: `python -m tasklog add TEXT`, `list` (with `--all`), `done ID`, `remove ID`, and a global `--state PATH` option.
- Implement robust state handling: atomic file writes to prevent partial JSON truncation, positive sequential integer IDs that remain stable after task deletions, input validation, and informative error messages on stderr.
- **Verification Focus**: Basic CLI argument parsing, fresh subprocess invocations, deterministic JSON file persistence, and bounded turn execution.

---

### 02. Reading Queue

- **Directory**: [`cases/02-reading-queue/project`](file:///C:/Users/saehwan/repos/rupi/cases/02-reading-queue/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/02-reading-queue/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/02-reading-queue/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_readqueue_http.py`](file:///C:/Users/saehwan/repos/rupi/cases/02-reading-queue/acceptance/test_readqueue_http.py)

**What it is & what to build**:
Build [`readqueue`](file:///C:/Users/saehwan/repos/rupi/cases/02-reading-queue/project/readqueue), a lightweight JSON HTTP service backed by SQLite for tracking personal reading queue entries.

- Implement server command: `python -m readqueue serve --db PATH --host HOST --port PORT`.
- Implement REST API endpoints: `GET /healthz`, `GET /items`, `GET /items/<id>`, `POST /items` (title, URL, optional tags), `PATCH /items/<id>` (updating title, URL, tags, or status), and `DELETE /items/<id>`.
- Implement relational schema migrations/initialization, deterministic JSON response envelopes, structured `4xx` error responses, and state preservation across server restarts.
- **Verification Focus**: Long-lived background processes, HTTP protocol handling, relational SQLite persistence, and restart recovery.

---

### 03. Event Outbox

- **Directory**: [`cases/03-event-outbox/project`](file:///C:/Users/saehwan/repos/rupi/cases/03-event-outbox/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/03-event-outbox/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/03-event-outbox/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_event_outbox.py`](file:///C:/Users/saehwan/repos/rupi/cases/03-event-outbox/acceptance/test_event_outbox.py)

**What it is & what to build**:
Build [`outbox`](file:///C:/Users/saehwan/repos/rupi/cases/03-event-outbox/project/outbox), a transactional outbox system consisting of an HTTP ingestion daemon and an independent bounded worker process.

- Implement HTTP service: `python -m outbox serve --db PATH --host HOST --port PORT` exposing `POST /events` with client-supplied `idempotency_key`, `topic`, and JSON `payload`, alongside `GET /events/<event_id>`.
- Implement bounded worker: `python -m outbox worker --db PATH --sink PROGRAM [--sink-arg ARG]... --once` which reads pending events from SQLite in insertion order and sends them to a child sink executable via a newline-delimited JSON (NDJSON) protocol over standard I/O.
- Record attempt counts, delivery statuses (`pending` vs. `delivered`), and error strings in SQLite.
- **Verification Focus**: Decoupled multi-process producer/worker architecture, transactional outbox pattern, client idempotency, and direct-argv IPC communication.

---

### 04. Webhook Inbox

- **Directory**: [`cases/04-webhook-inbox/project`](file:///C:/Users/saehwan/repos/rupi/cases/04-webhook-inbox/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/04-webhook-inbox/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/04-webhook-inbox/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_webhook_inbox.py`](file:///C:/Users/saehwan/repos/rupi/cases/04-webhook-inbox/acceptance/test_webhook_inbox.py)

**What it is & what to build**:
Build [`webhookinbox`](file:///C:/Users/saehwan/repos/rupi/cases/04-webhook-inbox/project/webhookinbox), an authenticated webhook receiver and leased delivery worker.

- Implement HTTP service: `python -m webhookinbox serve --db PATH --secret SECRET --host HOST --port PORT`. Validates `X-Webhook-Signature: sha256=<hex>` on incoming `POST /deliveries` using constant-time HMAC-SHA256 comparison, ensuring secret keys are never stored in SQLite or exposed in JSON.
- Implement leased worker: `python -m webhookinbox worker --db PATH --sink PROGRAM --lease-seconds SECONDS --once`. Claims deliveries under a timed lease. If a worker process crashes or exceeds the lease window, the expired lease can be reclaimed by a subsequent worker.
- **Verification Focus**: Cryptographic message authentication, secret isolation, time-bounded delivery leases, and crash-reclaim recovery ensuring at-least-once delivery.

---

### 05. Batch Relay

- **Directory**: [`cases/05-batch-relay/project`](file:///C:/Users/saehwan/repos/rupi/cases/05-batch-relay/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/05-batch-relay/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/05-batch-relay/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_batchrelay.py`](file:///C:/Users/saehwan/repos/rupi/cases/05-batch-relay/acceptance/test_batchrelay.py)

**What it is & what to build**:
Build [`batchrelay`](file:///C:/Users/saehwan/repos/rupi/cases/05-batch-relay/project/batchrelay), an authenticated batch scheduler that executes directed acyclic graph (DAG) job workflows.

- Implement HTTP service: `python -m batchrelay serve --db PATH --secret SECRET ...` accepting HMAC-signed `POST /batches` containing multiple jobs with explicit dependency declarations (`dependencies: [job_ids...]`). Validates graph integrity and rejects cycles.
- Implement worker: `python -m batchrelay worker --db PATH --sink PROGRAM ... --once`. Executes jobs strictly in topological dependency order.
- Implement failure cascading: retryable sink failures leave jobs in `pending` state; terminal sink failures immediately mark all downstream dependent jobs as `blocked`. Derives aggregate batch completion status.
- **Verification Focus**: Graph validation, topological dependency scheduling, terminal failure cascading, and leased execution.

---

### 06. Artifact Pipeline

- **Directory**: [`cases/06-artifact-pipeline/project`](file:///C:/Users/saehwan/repos/rupi/cases/06-artifact-pipeline/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/06-artifact-pipeline/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/06-artifact-pipeline/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_artifact_pipeline.py`](file:///C:/Users/saehwan/repos/rupi/cases/06-artifact-pipeline/acceptance/test_artifact_pipeline.py)

**What it is & what to build**:
Build [`artifactpipe`](file:///C:/Users/saehwan/repos/rupi/cases/06-artifact-pipeline/project/artifactpipe), an authenticated pipeline engine that adds inter-job data flow across a dependency DAG.

- Implement signed HTTP pipeline ingestion (`POST /pipelines`) and bounded worker execution.
- Implement data flow propagation: successful jobs persist a structured JSON `output` payload returned by the sink. Downstream jobs explicitly declare input references (`inputs: {"param": {"job": "job_a", "field": "result_key"}}`).
- The worker dynamically resolves references against upstream dependency outputs and injects resolved inputs into the child sink's input payload.
- **Verification Focus**: Explicit scalar data-flow across DAG nodes, output payload validation, and relational persistence of job artifacts.

---

### 07. Lease Cascade

- **Directory**: [`cases/07-lease-cascade/project`](file:///C:/Users/saehwan/repos/rupi/cases/07-lease-cascade/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/07-lease-cascade/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/07-lease-cascade/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_lease_cascade.py`](file:///C:/Users/saehwan/repos/rupi/cases/07-lease-cascade/acceptance/test_lease_cascade.py)

**What it is & what to build**:
Build [`leasecascade`](file:///C:/Users/saehwan/repos/rupi/cases/07-lease-cascade/project/leasecascade), a pipeline engine introducing barrier synchronization and fan-out/fan-in aggregation.

- Implement signed pipeline admission and worker execution extending the Artifact Pipeline model.
- Implement `barrier` jobs: a barrier job defines a selection rule (`barrier: {"select": "<field_name>"}`). It waits for all direct upstream dependencies to finish successfully, collects the selected field from each dependency's output in declared `depends_on` order, and feeds the resulting ordered collection into the barrier job.
- Missing selected fields cause an immediate terminal failure, properly cascading `blocked` status to downstream jobs.
- **Verification Focus**: Barrier synchronization, fan-out/fan-in collection aggregation, ordered dependency processing, and deterministic cascade failures.

---

### 08. Lease Fence

- **Directory**: [`cases/08-lease-fence/project`](file:///C:/Users/saehwan/repos/rupi/cases/08-lease-fence/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/08-lease-fence/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/08-lease-fence/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_lease_fence.py`](file:///C:/Users/saehwan/repos/rupi/cases/08-lease-fence/acceptance/test_lease_fence.py)

**What it is & what to build**:
Build [`leasefence`](file:///C:/Users/saehwan/repos/rupi/cases/08-lease-fence/project/leasefence), a pipeline service providing optimistic claim fencing to prevent zombie/stale worker races.

- Extend the pipeline and worker with a private per-claim fencing token (`claim_token`).
- When a worker claims a job, a unique, unguessable token is generated and recorded with the lease.
- When finalizing a job outcome (success, retry, or failure), the worker's mutation updates the row only if the `claim_token` still matches the active lease.
- If worker A claims a job but stalls past the lease duration, worker B reclaims it with a new token and succeeds. When worker A eventually returns, its late write is fenced out and rejected without corrupting worker B's state.
- **Verification Focus**: Distributed concurrency fencing, stale completion detection, split-brain mitigation, and state safety across worker lease reclamation.

---

### 09. Lease Receipt

- **Directory**: [`cases/09-lease-receipt/project`](file:///C:/Users/saehwan/repos/rupi/cases/09-lease-receipt/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/09-lease-receipt/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/09-lease-receipt/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_lease_receipt.py`](file:///C:/Users/saehwan/repos/rupi/cases/09-lease-receipt/acceptance/test_lease_receipt.py)

**What it is & what to build**:
Build [`leasereceipt`](file:///C:/Users/saehwan/repos/rupi/cases/09-lease-receipt/project/leasereceipt), an idempotent-sink pipeline service ensuring safe recovery across lost sink acknowledgements.

- Retain lease fencing and introduce stable, non-secret delivery keys (`delivery_key`) generated at job creation.
- Propagate the `delivery_key` with every sink execution attempt. Sinks record external side effects along with the delivery key and return a durable sink receipt.
- If a sink completes a side effect but the worker crashes before recording the acknowledgement, a subsequent reclaimed worker sends the same delivery key. The sink returns its cached receipt and output without re-executing the side effect.
- **Verification Focus**: Stable logical delivery identities, idempotent sink execution, durable receipt storage, and at-least-once recovery without duplicate side effects.

---

### 10. Receipt Ledger

- **Directory**: [`cases/10-receipt-ledger/project`](file:///C:/Users/saehwan/repos/rupi/cases/10-receipt-ledger/project)
- **Specification**: [`SPEC.md`](file:///C:/Users/saehwan/repos/rupi/cases/10-receipt-ledger/project/SPEC.md) | **Readiness & Guide**: [`README.md`](file:///C:/Users/saehwan/repos/rupi/cases/10-receipt-ledger/project/README.md)
- **Acceptance Oracle**: [`acceptance/test_receipt_ledger.py`](file:///C:/Users/saehwan/repos/rupi/cases/10-receipt-ledger/acceptance/test_receipt_ledger.py)

**What it is & what to build**:
Build [`receiptledger`](file:///C:/Users/saehwan/repos/rupi/cases/10-receipt-ledger/project/receiptledger), the capstone pipeline service featuring an append-only, tamper-evident operational audit ledger.

- Retain authenticated admission, DAG dependencies, barrier fan-in, lease fencing, and sink receipts.
- Implement an append-only cryptographic audit chain in SQLite: every pipeline admission, claim, attempt, completion, and failure atomically commits a canonical JSON event into an audit table.
- Link events with sequential IDs and SHA-256 hashes (`seq`, `prev_hash`, `hash`, `event_type`, payload). Secrets and private tokens are never leaked into the audit trail.
- Implement audit verification commands: `python -m receiptledger audit --db PATH --verify` (validates hash chain integrity and sequence continuity, detecting manual database tampering) and `python -m receiptledger audit --db PATH --tail COUNT`.
- **Verification Focus**: Cryptographic audit chains, same-transaction audit logging, tamper detection, CLI audit verification, and full end-to-end reliability.

---

## How to Test and Try Out rupi

### 1. Independent Python Test Suites

Each case includes both internal unit tests and an external acceptance oracle:

```bash
# Example: Running Case 01 tests
cd cases/01-task-ledger/toy-project
python -m unittest discover -s tests -p "test_*.py" -v
cd ..
python -m unittest discover -s acceptance -p "test_*.py" -v

# Example: Running Case 10 tests
cd cases/10-receipt-ledger/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
```

### 2. Using rupi with a Case

To test `rupi`'s code understanding, implementation, or debugging capabilities against any case:

```bash
# Run a bounded, non-mutating inspection turn
rupi run --config rupi.verify.config.json --cwd cases/01-task-ledger/toy-project \
  --prompt "Read SPEC.md and the project files. Summarize the state model and verification suite."

# Run an implementation or repair slice in a case workspace
rupi run --config rupi.config.json --cwd cases/02-reading-queue/project \
  --prompt "Read SPEC.md. Run tests and ensure all endpoints handle edge cases."
```
