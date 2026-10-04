# ADR-001: Filesystem storage, entity ownership, and integration seams

Status: accepted
Deciders: Emanuele Ciurleo (owner), AI agent
Applies to: #11 (model), #18 (seams), and all downstream phases

## Context

TucanoTime is a timesheet + invoicing app in the Tucano suite. Every sibling
(TucanoTestAPI/GUI) is **file-based with no database**, and the owner reaffirmed
that constraint. The app must stay portable (one folder on a volume), auditable
(human-readable JSON), and safe to extend with later integrations (SSO, payments,
accounting sync, calendar OAuth) that are deliberately **lowest priority to build
but first priority to design for**.

## Decision

### 1. No database — the filesystem is the store, the API is the only writer

- Every entity is one pretty-printed JSON document under `TUCANO_DATA_DIR`:
  ```
  customers/<id>.json
  customers/<id>/projects/<CODE>.json
  customers/<id>/projects/<CODE>/tasks/<TASK>.json   (Phase 1, #38)
  entries/<YYYY-MM-DD>/<id>.json
  invoices/<id>.json                                  (M2, #8)
  ```
  The on-disk tree mirrors the conceptual ownership (a project lives inside its
  customer; a task inside its project; entries in a per-day folder).
- Writes are atomic (temp file + rename) and serialised by a single process-local
  lock. Path components are only ever built from validated ids/codes, so request
  input can never traverse the tree.
- The GUI never reads storage; it is a pure presentation layer over the API.

### 2. Money and time are exact integers

- Rates and amounts are **minor units** (`u64` cents); hours are **hundredths**
  (`u32`). Floats never touch persisted values, so report and (future) invoice
  totals are exact and stable.

### 3. Rate & currency ownership (#11)

- A **customer** holds the *default* currency and *default* rate.
- A **project** always carries its **own** currency and rate, required at creation
  and prefilled (in the GUI) from the customer. Billing reads the project's values
  directly — there is no silent per-entry fallback to the customer.
- The customer default is therefore a *creation-time default*, not a billing rule.
- **Back-compatibility:** project documents written before this change may omit the
  fields. On read, missing currency/rate are resolved from the owning customer
  (self-healing on next write). A read never fails over a legacy document.
- **Invoicing implication (#8):** an invoice is single-currency. If a period mixes
  projects whose currency differs from the customer's, invoice generation rejects
  with a clear error rather than silently converting.

### 4. Integration seams — design now, build last (#18)

External concerns sit behind narrow traits so a provider is an adapter, never a
domain change. To avoid speculative abstraction, a port is created only when a
near-term consumer exists; the rest are named here as the seam to implement when
their Phase 6 ticket is picked up:

| Concern | Port | Consumer / when |
| --- | --- | --- |
| Time source | `Clock` | timer #14, reminders #22, budgets #30 — **now** |
| In-app/notify | `NotificationSender` | reminders #22 — **now** |
| Email | `EmailSender` | invoice email #35 — Phase 6 |
| Payments | `PaymentProvider` | Stripe/PayPal #34 — Phase 6 |
| Accounting | `AccountingSync` | QBO/Xero #33 — Phase 6 |
| Identity | `IdentityProvider` | SSO #32 (local-password adapter first, #19) |
| Calendar | `CalendarSource` | ICS #15 now; OAuth #36 Phase 6 |
| Rates | `RateSource` | multi-tier #21 |

Two hard requirements that must exist **before** Phase 6 so imported/derived data
is traceable and lockable:

- Entries carry a **`source`** (`manual | timer | calendar-import`).
- A single shared **lock check** (used by invoices #8, submissions #16,
  reimbursements #24): an entry locked by any holder rejects edits/deletes with 409.

## Consequences

- Portable, inspectable data; trivial backup = copy the folder.
- No provider SDK is pulled into Phases 1–5; Phase 6 work is additive adapters.
- The rate-ownership change (#11) is a breaking API change (project fields become
  required) — acceptable pre-1.0, documented in its PR, with legacy reads preserved.
- **Concurrency (#62):** every mutation takes a process-local mutex **and** an
  advisory exclusive `flock` on `.tucanotime.lock` (via `fs2`), retried to a 5 s
  deadline then surfaced as **503 + `Retry-After`**. Writes are atomic (tmp +
  rename). This serialises concurrent writers across processes sharing the volume,
  and read-modify-writes (e.g. invoice numbering via `Store::create_invoice`)
  hold the lock end-to-end so numbers can't collide.
