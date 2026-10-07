# TucanoTime

A timesheet system: record time on a daily and weekly basis, organised by
**customer** and **project code**, each carrying its own **currency and hourly
rate**. Built as part of the [Tucano Technology](https://github.com/TucanoTechnology)
suite — file-based (no database), one container, browser GUI.

## Features

- **First-run wizard** — new users land on a guided modal that creates the first customer and project and drops them straight into a ready entry form; the 🚀 sidebar entry reopens it any time (#111).
- **Day view** — add, edit and delete line items for a single date.
- **Week grid** — Mon–Sun overview per customer/project; click a cell to add or adjust.
- **Customers & project codes** — first-class entities; a customer sets the default currency/rate and each project carries its own currency and rate.
- **Exact money** — rates in minor units and hours in hundredths; no floats on persisted values.
- **Reports** — totals grouped by customer, project or ISO week; **CSV export** (formula-injection safe).
- **Invoices** — generate a draft from a period's billable work, snapshotting each entry's rate; issuing **locks** its entries, **archives a PDF** for download, email attachment (#113) and audited PDF copies to third parties such as the accountant (#112); record **full or partial payments** (#114, a per-invoice ledger with a partly-paid state) or **write off** uncollectable balances, and an **outstanding/overdue** dashboard tracks the balance still owed.
- **Invoice overview** — open/paid tiles, a year-navigable monthly Open-vs-Paid chart, Open/All lists with search, customer/project/date filters, sortable columns and column visibility — row selection (click or keyboard) opens the document preview (#135).
- **Tracked-work wizard** — a staged create-from-time flow (customer → period & projects with uninvoiced-hours per project → live review via `POST /invoices/preview`) that saves a draft only after the review; project-scoped generation excludes other projects' work (#134).
- **Retainers** — advance funds per customer/project as an append-only ledger: opening, add funds, draws with reason, closable; balances are always reconciled from transactions, never stored (#144).
- **Appearance & messages** — invoice document accent (printed into the archived PDF) and sender display name / Reply-To, all honored by the real transports — no inert settings; credentials stay in the vault (#146).
- **Catalog & labels** — reusable Product/Service line types prefill manual invoice lines (archive keeps history); field labels rename the document's wording without touching API names (#147).
- **Invoice templates** — org-wide subject/body/footer with `%variable%` placeholders (markdown subset), per-customer notes, subject overrides and payment terms (Upon Receipt / NET 15–45 / custom) driving the due date at issue (#116).
- **API-level integrations** — hosted checkout links (#34) and accounting sync (#33) remain available on the API and background retry job, but are no longer surfaced as per-row invoice buttons (#129).
- **Expenses** — record costs per project with categories, billable flags, and optional attached receipts; billable expenses roll into invoices (#25).
- **Timesheet submissions** — submit a week for approval; submitted/approved weeks **lock** their entries (shared lock seam); rejecting releases them.
- **Also shipped** — running **timer** and month calendar of logged time (#14/#140); imported **calendar events** with Google OAuth (#15/#36); **SSO sign-in** (OIDC/signed-token, JIT provisioning — Beta adapters, #32); per-user **notifications** (#22/#52); **recurring invoice schedules** (weekly/monthly/quarterly, drafts still need an explicit issue, #26); **expense reimbursement claims** (#24); **budgets** burn alerts + per-project budget vs actual (#30) and **profitability** reporting (revenue vs labour cost, admin-only, #29); **email delivery** of invoices with the archived PDF attached (#35/#130); role-based access — members see only their own time (#51); an **audit log** (#52) and an operator **backup/restore CLI** on the same image (#94).

## Quick start (Docker)

```sh
docker build -t tucanotime .
docker run -d -p 8080:8080 -v tucanotime-data:/data --name tucanotime tucanotime
```

Open <http://localhost:8080/> — the API docs are at <http://localhost:8080/docs>.
All data lives in the mounted volume; back it up like any document folder.

## Persistence & upgrades (#94)

**Everything an installation knows lives in one place: the data dir**
(`TUCANO_DATA_DIR`, `/data` in the image). Containers are disposable; the
volume is the installation:

| What | Where | Survives `up -d` / image update |
|---|---|---|
| Users, entries, invoices, expenses, notifications… | `<data>/…` JSON documents | ✅ (volume) |
| Integration credentials (Settings tab) | `<data>/secrets.bin`, AES-256-GCM | ✅ — **requires** the same `TUCANO_SECRET_KEY[_FILE]` |
| Runtime settings (reminder cadence, SSO group/domains, OAuth redirect, provider base URLs, demo flags…) | `<data>/config.json`, env > file > default | ✅ (volume; no env needed) |
| Session signing key | `<data>/session.key`, auto-created 0600 on first boot | ✅ — restarts no longer log everyone out |
| Scheduler last-run state, revocations, audit log | `<data>/…` | ✅ |

Precedence everywhere is **explicit env → `config.json` → built-in
default**, so you can pin values with env at the OS layer or persist them on
the volume via `PUT /admin/config` / `GET /admin/config` (masked to the
non-secret whitelist by construction).

Secret files (bind-mount / Docker secrets) work for both keys:
`TUCANO_SESSION_SECRET_FILE` and `TUCANO_SECRET_KEY_FILE`. If the vault store
exists on disk but the key is missing or wrong, the app **refuses to start**
with a clear message — a silently disabled Settings tab has historically
looked like lost data, and we'd rather stop than mislead.

### Upgrading an image

```sh
docker compose pull && docker compose up -d   # nothing to reconfigure
```

### Backup / restore

```sh
docker compose run --rm tucanotime-cli --backup /backups/monday.tar.gz
# restore (stop the server first; refuses non-empty dirs without --force):
docker compose stop tucanotime
docker compose run --rm tucanotime-cli --restore /backups/monday.tar.gz
docker compose up -d tucanotime
```

Archives are plain `tar.gz` with a `manifest.json` (version + sha256 per
file); restores verify checksums before writing, and warn if the archive
carries a vault store while no key is configured.

### Bind mounts

The container runs as uid **10001**; bind-mounted host dirs need to be
writable by it (`chown -R 10001:10001 ./data`), or just use a named volume.

## Authentication

TucanoTime is single- or multi-user with **session-cookie auth** (argon2-hashed
passwords, HMAC-signed HttpOnly cookies). No users exist initially:

- On first visit the app shows a **create-administrator** form (the first account
  becomes an `admin`). After that, login is required for all data.
- Sessions survive restarts out of the box: the signing key is
  `TUCANO_SESSION_SECRET` / `TUCANO_SESSION_SECRET_FILE` when set, else an
  auto-created `<data>/session.key` on the volume (#94) — no operator setup
  needed, and container updates keep everybody signed in.
- Cookies carry `Secure` when `TUCANO_ENV=production` **and** are plain-HTTP-LAN
  friendly via the independent `TUCANO_SECURE_COOKIES=0` axis (A5): the key
  controls secret-strength policy, the flag controls only the cookie attribute.

Environment variables (all optional; precedence env > `config.json` > default):

| Variable | Effect |
|---|---|
| `TUCANO_DATA_DIR` | data folder (`/data` in the image) |
| `TUCANO_PORT` | listen port, default `8080` |
| `TUCANO_SESSION_SECRET[_FILE]` | session signing key; unset ⇒ auto-persisted `<data>/session.key` |
| `TUCANO_SECRET_KEY[_FILE]` | 32-byte vault key; unset ⇒ vault disabled (fail-closed) |
| `TUCANO_ENV=production` | strict secret policy + `Secure` cookies by default |
| `TUCANO_SECURE_COOKIES=0` | drop the `Secure` flag for plain-HTTP serving on a trusted LAN |
| `TUCANO_MAX_DOCS` | per-collection document cap (abuse guard) |

```sh
docker run -d -p 8080:8080 \
  -e TUCANO_SESSION_SECRET="$(openssl rand -hex 32)" \
  -v tucanotime-data:/data --name tucanotime tucanotime
```

Roles: `admin` (user management) and `member` (timesheet work).

**Integration credentials (#77):** admins store provider secrets (Stripe, OAuth,
SMTP, ICS feed URLs) in the **Settings** tab; they are encrypted at rest with
AES-256-GCM under `TUCANO_SECRET_KEY` (a 32-byte env var) and never returned or
logged. Without `TUCANO_SECRET_KEY` the vault is disabled (fail-closed).
`secrets.bin` holds the encrypted blob.

## Development

```sh
cargo run                     # serve GUI + API on :8080 (data in ./data)
cargo test                    # unit + contract tests
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Storage is file-based JSON under `TUCANO_DATA_DIR` (the API is the only
writer). The canonical layout — customers/projects/tasks, entries by day,
invoices (+ archived PDFs and the `.seq.json` number ledger), users,
categories, expenses, submissions, and the `config.json` / `session.key` /
`secrets.bin` / `audit.log` / `revoked.json` / `scheduler.json` state files —
is documented in [`AGENTS.md`](AGENTS.md) "Architecture invariants" so there
is exactly one list to keep current.

The REST surface is defined by [`openapi.json`](openapi.json) and served at
`/openapi.json`; change both together. See [`AGENTS.md`](AGENTS.md) for the
rules AI agents follow in this repository.

## Architecture decision records

- [ADR-001 — Filesystem storage, entity ownership, and integration seams](docs/architecture/adr-001-filesystem-and-seams.md)
- [ADR-002 — E-invoicing standard and provider scope](docs/architecture/adr-002-einvoicing-scope.md) (decision record for #148: EN 16931 core; Peppol BIS 3.0 UBL + Factur-X generation at v1; transport only through a provider port — state-clearance schemes and PDP certification are explicitly out of scope)

## License

[GNU Affero General Public License v3.0](LICENSE).
