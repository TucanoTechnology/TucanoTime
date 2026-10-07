# AI Agent Rules — Tucano Time

Rules and guidelines for AI agents working on Tucano Time.

## Shared rules

Organisation-wide rules live in [`AgentRules/`](AgentRules/) and apply to every repository.
Read them before making changes. This file records only what is specific to this repository.

**`AgentRules/` is synced automatically from
[TucanoAgentRules](https://github.com/TucanoTechnology/TucanoAgentRules), the single source of
truth shared with [TucanoTestAPI](https://github.com/TucanoTechnology/TucanoTestAPI) and
[TucanoTestGUI](https://github.com/TucanoTechnology/TucanoTestGUI). Do not edit
files under `AgentRules/` in this repository — submit changes against TucanoAgentRules instead and
they will arrive here as an automated pull request.**

---

## Repository-specific rules

### What this project is

TucanoTime is a timesheet system: time entries recorded per day/week against a
customer and project code, with per-customer and per-project currency and hourly
rate. A single Rust binary serves the REST API, the browser GUI and the API docs;
data is file-based JSON (no database), mirroring the TucanoTest suite philosophy.
The GUI is a presentation layer and nothing more — it never reads storage.

### Architecture invariants

- Money is **minor units** (`u64` cents) and hours are **hundredths** (`u32`);
  floats never touch persisted values. Rate resolution: a project always
  carries its own currency + rate (required, #11); the customer holds the
  *default* used only to prefill new projects and to resolve legacy documents
  (see `domain::effective_rates`, `domain::project_from_bytes`).
- Tasks carry no currency or rate overrides. Legacy task billing fields are
  ignored on load; task entries use the existing person/project/customer rate
  resolution and project/customer currency resolution.
- Storage layout below `TUCANO_DATA_DIR` (the API is the only writer):
  `customers/<id>.json`, `customers/<id>/projects/<CODE>.json`,
  `customers/<id>/projects/<CODE>/tasks/<TASK>.json` (optional task tier, #38),
  `entries/<YYYY-MM-DD>/<id>.json`, `invoices/<id>.json`, `users/<id>.json`,
  `categories/<id>.json`, `expenses/<id>.json`, `submissions/<id>.json`,
  plus `audit.log` (#52), `revoked.json` (#45), `scheduler.json` (#61),
  `secrets.bin` (#77, AES-256-GCM encrypted), `config.json` + `session.key`
  (#94), `.server.lock` (single-instance guard), the `retainers/`, `claims/`,
  `schedules/`, `timers/`, `items/` and `notifications/` doc folders,
  `sync/accounting.json` (#33), archived
  `invoices/<id>.pdf` (#113), `invoices/.seq.json` (number ledger, B3) and
  `*.idx.*` side indexes (D3). Atomic writes (tmp + rename).
- **Secret vault (#77):** admin-entered integration credentials are encrypted at
  rest with `TUCANO_SECRET_KEY` (32 bytes) and never returned to clients (masked
  hints only) or logged. Fail-closed: unset key ⇒ vault disabled.
- **Locking (#18 seam):** entry edits/deletes consult `CombinedLocks`, which
  composes `InvoiceLock` (entries on an *open* invoice — `status.is_open()` is
  exactly `issued | partly_paid`, #8/#114 — so settling or writing off an
  invoice **releases** its entries: deliberate lifecycle, not an oversight.
  A closed invoice can never change (rates/hours are snapshotted at issue),
  while the timesheet stays correctable; a partly-paid invoice is still
  collectable and keeps its entries frozen) and `SubmissionLock` (entries in a
  submitted/approved week, #16). Reads are never blocked. If the lock state
  cannot be verified the write is refused fail-closed with 503
  `lock_unavailable` (#185) — never allowed because the check was inconclusive.
  Reimbursement claims (#24) lock their expenses via a direct scan in
  `api::expenses`, not through this seam (see the #190 follow-up note).
- Every payload is validated against the contract before any write
  (`deny_unknown_fields` + `domain::validate_*`); no partial persistence.
  Errors use the single JSON shape in `openapi.json`; responses never expose
  paths or raw filesystem errors.
- The **contract is `openapi.json`** (checked in, served at `/openapi.json`,
  rendered at `/docs`). Route changes and contract changes land in the same
  commit. Range queries are capped at 400 days.
- `web/` is dependency-free vanilla HTML/CSS/JS embedded via rust-embed (no
  build step). All DOM data is inserted with `textContent`, never `innerHTML`.
- **Auth (#19):** users are file-based (`users/<id>.json`), passwords argon2id,
  sessions are HMAC-signed HttpOnly cookies (`tt_session`). Data routes require a
  valid session; `/users` requires `admin`. First-run `/auth/bootstrap` creates
  the initial admin only while no users exist. Key: `TUCANO_SESSION_SECRET[_FILE]`
  else the auto-created `<data>/session.key`, so restarts keep sessions (#94);
  the `Secure` cookie flag is the independent `TUCANO_SECURE_COOKIES` axis
  (default on iff `TUCANO_ENV=production`; `=0` allows plain-HTTP LAN serving,
  review A5). `password_hash` is never serialised to clients.
- **RBAC / private-per-user (#51):** entries and expenses carry `user_id` (the
  author). A **member** sees/edits only their own records; an **admin** sees all
  and owns `/invoices` (admin tier) and submission approval. Enforce via
  `visible_to()` on reads and by ownership check on mutations (a non-owned record
  returns 404, not 403, to avoid leaking existence). Customers/projects/tasks/
  categories are shared org-wide reference data (any authenticated user).
- **Invoices (#8):** generated from a period's billable, not-yet-invoiced entries; each line snapshots the resolved rate at generation, so a later rate change never rewrites an issued invoice. Issuing sets `status=issued`, which the `InvoiceLock` (a `lock::EntryLock` provider wired in `AppState`) uses to reject edits/deletes of referenced entries with 409. Draft invoices do not lock. Invoice number is sequential (`INV-0001`).

### Commands

| Task | Command |
| --- | --- |
| Run (GUI + API on :8080, data in `./data`) | `cargo run` |
| All tests (domain units + HTTP contract) | `cargo test --all-targets` |
| Format check / apply | `cargo fmt --all -- --check` / `cargo fmt --all` |
| Lint (CI treats all clippy lints as errors) | `cargo clippy --all-targets -- -D warnings` |
| Latest version of a crate | `cargo info <crate>` (or crates.io API) |
| Container image | `docker build -t tucanotime .` |
| Container run | `docker run -d -p 8080:8080 -v tucanotime-data:/data tucanotime` |

The toolchain is pinned by `rust-toolchain.toml` and the Dockerfile/CI images
carry the same version; bump all three in one intentional PR.

### Test policy beyond the shared rules

- Behaviour changes ship with contract tests in `tests/contract.rs` (in-process
  router, temp data dir); cover the invalid shapes too — rejection without
  persistence is part of the contract.
- Accessibility: the GUI keeps keyboard-navigable tabs, labelled fields,
  `aria-live` announcements and passing contrast in light/dark themes; check
  new views against the same bar.


<!-- shared-rules-table:begin -->
| Area | Rules |
| --- | --- |
| API & data contracts | [`AgentRules/coding/api-and-data-contracts.md`](AgentRules/coding/api-and-data-contracts.md) |
| Branching & git | [`AgentRules/coding/branching-and-git.md`](AgentRules/coding/branching-and-git.md) |
| Code review | [`AgentRules/coding/code-review.md`](AgentRules/coding/code-review.md) |
| Dependencies | [`AgentRules/coding/dependencies.md`](AgentRules/coding/dependencies.md) |
| Principles | [`AgentRules/generic/principles.md`](AgentRules/generic/principles.md) |
| Ticket template | [`AgentRules/project-management/ticket-template.md`](AgentRules/project-management/ticket-template.md) |
| Ticket workflow | [`AgentRules/project-management/workflow.md`](AgentRules/project-management/workflow.md) |
| Security | [`AgentRules/security/security.md`](AgentRules/security/security.md) |
| Contract tests | [`AgentRules/test/contract.md`](AgentRules/test/contract.md) |
| Performance tests | [`AgentRules/test/performance.md`](AgentRules/test/performance.md) |
| Browser & end-to-end tests | [`AgentRules/test/ui.md`](AgentRules/test/ui.md) |
| Unit tests | [`AgentRules/test/unit.md`](AgentRules/test/unit.md) |
<!-- shared-rules-table:end -->
