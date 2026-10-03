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
- Storage layout below `TUCANO_DATA_DIR` (the API is the only writer):
  `customers/<id>.json`, `customers/<id>/projects/<CODE>.json`,
  `customers/<id>/projects/<CODE>/tasks/<TASK>.json` (optional task tier, #38),
  `entries/<YYYY-MM-DD>/<id>.json`. Atomic writes (tmp + rename).
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
  the initial admin only while no users exist. Key from `TUCANO_SESSION_SECRET`
  (ephemeral if unset); `TUCANO_ENV=production` sets the `Secure` cookie flag.
  `password_hash` is never serialised to clients.
- Invoices (M2, planned): snapshot entries + reference entry ids and lock
  referenced entries from edits/deletes. Keep entry ids stable — invoices
  depend on them.

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
