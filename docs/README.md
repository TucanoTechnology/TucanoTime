# TucanoTime documentation

Human documentation for operators, administrators and developers. Start with the
guide that matches what you are trying to do.

## Getting started

- [Getting started](getting-started.md) — what TucanoTime does, first run, roles,
  the GUI map and your first timesheet/invoice.

## Configuration

- [Configuration](configuration.md) — every environment variable and
  `config.json` key, precedence, secret-file recipes and the credential vault.

## Operations

- [Operations](operations.md) — upgrades, backup and restore, bind mounts,
  container hardening, health, logs and the scheduler.
- [Unraid runbook](../deploy/unraid/README.md) — the canonical Unraid template
  and step-by-step runbook.

## Security

- [Security](security.md) — sessions, passwords, roles, SSO, the credential
  vault, audit logging, rate limiting and record locks.

## Storage and retention

- [Storage and retention](storage-and-retention.md) — the data layout, money and
  time invariants, invoice permanence and entry archival.

## API

- [API](api.md) — the `/api` mount, the OpenAPI contract, GUI routes, status
  conventions and pagination.

## Invoicing and expenses

- [Invoicing and expenses](invoicing-and-expenses.md) — the invoice lifecycle,
  expenses, claims, retainers, catalog, templates and accounting sync.
- [Harvest import status](harvest-import.md) — the read-only parser milestone
  and remaining safeguards before financial records can be imported.

## Testing

- [Testing](testing.md) — unit, contract, GUI and browser suites, plus CI gates.

## Development

- [Development](development.md) — local workflow, live reload, contribution and
  release process, repository layout and licence.

## User management

- [User management](user-management.md) — accounts, roles, passwords and SSO
  provisioning.

## UI design record

- [Ubuntu/Vanilla design record](ubuntu-design.md) — the design rationale and
  component backlog for the current GUI.

## UI migration tracker (historical)

- [Ubuntu/Vanilla migration plan](ubuntu-migration-plan.md) — the page-by-page
  tracker kept as a historical record.

## Architecture decision records

- [Architecture decision records](architecture/README.md) — ADR index with
  status labels.

## Code style

- [Code style](coding-style.md) — the Rust documentation and style bar.

## Historical reviews (frozen)

- [Review archives](reviews/README.md) — point-in-time reviews kept for the
  record and deliberately not updated.

---

Last verified against TucanoTime 0.1.41.
