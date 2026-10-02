# TucanoTime

A timesheet system: record time on a daily and weekly basis, organised by
**customer** and **project code**, each carrying its own **currency and hourly
rate**. Built as part of the [Tucano Technology](https://github.com/TucanoTechnology)
suite — file-based (no database), one container, browser GUI.

## Features

- **Day view** — add, edit and delete line items for a single date.
- **Week grid** — Mon–Sun overview per customer/project; click a cell to add or adjust.
- **Customers & project codes** — first-class entities; a project may override its customer's currency or rate.
- **Exact money** — rates in minor units and hours in hundredths; no floats on persisted values.
- **Reports** — totals grouped by customer, project or ISO week; **CSV export** (formula-injection safe).
- **Invoices (planned, M2)** — snapshot entries into an invoice and lock the entries used.

## Quick start (Docker)

```sh
docker build -t tucanotime .
docker run -d -p 8080:8080 -v tucanotime-data:/data --name tucanotime tucanotime
```

Open <http://localhost:8080/> — the API docs are at <http://localhost:8080/docs>.
All data lives in the mounted volume; back it up like any document folder.

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
