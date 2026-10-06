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
- **Tracked-work wizard** — a staged create-from-time flow (customer → period & projects with uninvoiced-hours per project → live review via `POST /invoices/preview`) that saves a draft only after the review; project-scoped generation excludes other projects' work (#134).
- **Invoice templates** — org-wide subject/body/footer with `%variable%` placeholders (markdown subset), per-customer notes, subject overrides and payment terms (Upon Receipt / NET 15–45 / custom) driving the due date at issue (#116).
- **API-level integrations** — hosted checkout links (#34) and accounting sync (#33) remain available on the API and background retry job, but are no longer surfaced as per-row invoice buttons (#129).
- **Expenses** — record costs per project with categories, billable flags, and optional attached receipts; billable expenses roll into invoices (#25).
- **Timesheet submissions** — submit a week for approval; submitted/approved weeks **lock** their entries (shared lock seam); rejecting releases them.

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
- Set `TUCANO_SESSION_SECRET` (e.g. `openssl rand -hex 32`) so sessions survive
  restarts; unset, a random per-process key is used (restart logs everyone out).
- `TUCANO_ENV=production` marks session cookies `Secure` (serve over TLS).

```sh
docker run -d -p 8080:8080 \
  -e TUCANO_SESSION_SECRET="$(openssl rand -hex 32)" \
  -v tucanotime-data:/data --name tucanotime tucano-time
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

Storage layout under `TUCANO_DATA_DIR` (the API is the only writer):

```
customers/<id>.json                       # customer record
customers/<id>/projects/<CODE>.json       # project codes live inside their customer
entries/<YYYY-MM-DD>/<id>.json            # one folder per day, one file per entry
```

The REST surface is defined by [`openapi.json`](openapi.json) and served at
`/openapi.json`; change both together. See [`AGENTS.md`](AGENTS.md) for the
rules AI agents follow in this repository.

## License

[GNU Affero General Public License v3.0](LICENSE).
