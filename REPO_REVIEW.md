# TucanoTime — Full Repository Review (2026-10-06)

Companion document for issue #183. Supersedes the provisional notes files created earlier
on this branch. Every finding below was re-verified against HEAD `7f09701` with concrete
file:line evidence.

## 1. Baseline

| Item | Value |
| --- | --- |
| Branch / commit | `main` @ `7f09701` ("feat: adopt Ubuntu typography and Vanilla components (#181) (#184)") |
| Build | `cargo build` OK |
| Tests | `cargo test --all-targets`: 122 unit + 123 contract — all pass |
| Format / lint | `cargo fmt --check` and `cargo clippy -D warnings` clean |
| Runtime performing the review | OpenCode agent harness. The ticket requested a local Qwen Coder 30B; this environment runs a hosted Qwen model (`qwen3.8-flash` via the `alibaba-token-plan` provider). Quantization/context of a local 30B checkpoint is therefore **not applicable** — recorded honestly rather than guessed. All review input stayed local to this machine's repo checkout (no third-party uploads beyond the configured model endpoint). |
| Contract check | Every route in `src/lib.rs` has a matching `openapi.json` path and vice versa (verified programmatically) |
| Toolchain | 1.98.0 consistent across `rust-toolchain.toml`, `Dockerfile`, `.github/workflows/ci.yml` |
| Vendored assets | sha256 of all three Ubuntu woff2 fonts match `web/vendor/ubuntu/manifest.json`; `gui-test/build-ubuntu-assets.mjs` regenerates hashes reproducibly |

## 2. Language responsibility map (assessed: correct, keep)

- **Rust** — API, domain, storage, scheduler, integrations, embedded asset serving. Sole build/runtime language.
- **Semantic HTML / CSS / vanilla JS** — GUI under `web/`, dependency-free, rust-embed, no build step.
- **Sass + Node (jsdom)** — only for reproducibly compiling the pinned Vanilla assets (`gui-test/ubuntu-components.scss`) and the GUI test harness.
- **YAML/TOML** — CI, dependabot, deny, compose configuration.

Assessment against the ticket: this split is the most suitable option for a single-binary,
file-backed product; no evidence supports adding TypeScript/React/build tooling or any other
language. No language/framework migration proposed or made.

## 3. File inventory and disposition (102 tracked files)

Full `git ls-files` listing reviewed; `data/` is git-ignored (runtime files only — an earlier
draft of this document mistakenly inventoried them).

| Group | Files | Disposition |
| --- | --- | --- |
| `src/*.rs`, `src/api/*.rs` | 37 | **Keep.** All modules are wired in `src/lib.rs`; no orphan modules found (each has live callers). Size/maintainability findings R-BE-*. |
| `tests/contract.rs` | 1 | **Keep.** 123 in-process HTTP tests. |
| `web/` app assets | `index.html`, `app.js`, `styles.css`, `theme.js` | **Keep.** No `innerHTML` (verified; only comments mention it). Findings R-GUI-*. |
| `web/` docs UI | `swagger.html`, `swagger-ui-bundle.js`, `swagger-ui.css` | **Keep** — vendored deliberately (review A9 fix); referenced by the `/docs` route in `src/lib.rs:375`. Not orphans despite having no import. |
| `web/vendor/ubuntu/` | 6 assets + 4 licences + `manifest.json` + pinned `vanilla-framework-4.59.0.tgz` | **Keep** — all referenced by `manifest.json`, `build-ubuntu-assets.mjs`, and served via rust-embed (`index.html:8`). Licences/corresponding source required for traceability. |
| `gui-test/` | 6 scripts + package/lock + README | **Keep**; one drift finding R-DOC-8. |
| Docs root + `docs/` | `README.md`, `AGENTS.md`, `CODE_REVIEW.md`, 4 `docs/*.md`, 2 ADRs | **Keep with updates** (R-DOC-*). `CODE_REVIEW.md` is a historical record whose disposition is now complete except C3 — do not delete. |
| `AgentRules/` | 14 | **Keep, never edit here** (synced from TucanoAgentRules). |
| CI/config | `.github/*`, `Dockerfile`, `docker-compose.yml`, `deny.toml`, `.gitleaks.toml`, `Cargo.*`, `rust-toolchain.toml`, `.gitignore`, `LICENSE` | **Keep**; minor hygiene R-DOC-9/10/11. |
| `openapi.json` | 1 | **Keep.** Accurate against routes. |

**Orphan search method:** for each non-imported file we checked rust-embed serving paths
(`src/lib.rs::static_assets`), route handlers, `include_str!`, asset manifests, build scripts,
CI steps and docs. No file qualified as removable without human sign-off. **Uncertain cases
retained for maintainer review:** none — all candidates resolved above.

## 4. Findings catalog (prioritized)

Severity: HIGH (wrong results/data-loss/invariant break), MED (latent bug/maintainability
risk), LOW (hygiene). Each finding lists evidence, risk, and the focused check that must
accompany the fix. Bug tickets are separated from refactor tickets per the review rules.

### R1 — Storage/locking fail-open paths (correctness bug) · HIGH → ticket #185

1. `src/store.rs:983` `delete_customer`: `let projects = self.list_projects(id).unwrap_or_default();`
   — a store error is swallowed, the guard sees zero projects, and `:1003`
   `remove_dir_all(customers/<id>/)` **deletes project documents it failed to read**.
   Risk: silent data loss. Fix: propagate (`?`). Check: corrupt project doc ⇒ delete errors and tree intact.
2. `src/lock.rs:80` / `:109`: `self.store.list_invoices().ok()?` — any store error maps to
   `None` = "not locked", so with an unreadable (e.g. mid-write/issued) invoice document the
   entry edit/delete proceeds, breaking the #8/#18 issued-invoice lock invariant.
   Fix: fail closed (surface an error ⇒ 503, never 2xx). Check: corrupt one invoice file,
   edit a referenced entry, expect non-2xx.
3. `src/api/invoicing.rs:723-778` `payment_webhook`: the idempotency check
   (`payments.iter().any(|p| p.reference == reference)`) reads an **unlocked** snapshot;
   `store::record_payment` (`store.rs:1423-1453`) never re-checks reference uniqueness under
   the write lock. Two concurrent deliveries of the same event double-post payments.
   Fix: dedup inside the locked record. Check: replay same signed webhook concurrently ⇒ one ledger entry.

### R2 — Reports sum money across currencies (correctness bug) · HIGH → ticket #186

- `src/report.rs:117-121` comment claims grouping by customer/project "never mixes
  currencies" — false since #11: project currency is independent (`domain::effective_rates`
  returns the project's currency, `src/domain.rs:1461`). Only `Group::Week` keys rows by
  currency (`report.rs:135-137`); `Customer`/`Project`/`Person` rows add EUR-cents +
  USD-cents into one `amount_minor`. Same pattern in `invoice_report` (`report.rs:303`) and
  `summarise_profit` (`report.rs:211-244`).
- Related: `src/accounting.rs:177-179` sends QBO invoice amounts as JSON **floats**
  (`doc_amount`) while payments use the decimal **string** helper (`qbo_amount`, :173-175) —
  two encodings for the same provider.
- Check: unit test — one customer, two projects with different currencies ⇒ two summary rows;
  provider payload encoding identical for invoice and payment totals.

### R3 — Request-validation gaps and error-shape drift (correctness bug set) · MED → ticket #187

1. `src/domain.rs:613` `InvoiceLine` lacks `deny_unknown_fields` yet is used as request-body
   element by `PUT /invoices/{id}` (`api/invoicing.rs:1264`) — violates the validation
   invariant on that write path.
2. Manual-line bounds implemented twice with different rejection behaviour: `domain.rs:717`
   `validate_manual_lines` vs `api/invoicing.rs:1408-1444` (`update_invoice_draft` silently
   `continue`s bad lines).
3. `POST /claims` (`api/expenses.rs:319-334`) doesn't dedup `expense_ids` — a repeated id
   doubles its amount in the persisted claim total.
4. `ProjectInput` budget fields persist unbounded (`domain.rs:1665-1689`), unlike every
   other money input (cap `100_000_000`); `budgets.rs:79` then converts to f64.
5. `api/entries_reports.rs:14-33` (`.ok().flatten()`) and `api/invoicing.rs:179`
   (`org_for().unwrap_or_default()`) map store failures to 422/silent fallback.
6. `api/retainers.rs`: `store.rs:647` erases `RetainerError` into `StoreError::Io`, callers
   re-parse by substring (:150-166), and `close_retainer` (:204-211) has no rescue ⇒ a second
   close returns **500** instead of 409.
- Check: contract tests for each (unknown line field ⇒ 422; duplicate expense id ⇒ 422;
  second close ⇒ 409; both manual-line endpoints reject identically).

### R4 — GUI boot and async/state correctness bugs · HIGH → ticket #188

1. `web/app.js:63-81` `el()` ignores a top-level `style:` option; the invoice chart passes
   `style` at top level (:2317-2326) ⇒ computed bar heights never reach the DOM — the chart
   renders at zero height (functional bug shipped with #184's design work).
2. Member login boot-fails: `startApp` (`:4492-4505`) awaits `refreshInvoices()` in
   `Promise.all`, `GET /invoices` is admin-tier (`src/lib.rs:163,195`), the un-caught
   rejection aborts boot (title/version/timers never wire).
3. `inv-f-customer` is missing from the picker refresh list (`app.js:1847`), so the dashboard
   customer filter (#135) can never filter — dead control.
4. `writeOffInvoice` (`:2073-2090`) doesn't call `invalidateLockCache()` although write-off
   releases entry locks (`lock.rs:85` locks only `is_open()`; `domain.rs:600-602`) — stale
   locked UI for up to the 5s TTL; invariant documented at `:960-962`.
5. Missing stale-response guards on `refreshDay` (:651), `refreshTimeCal` (:456),
   `fillTaskSelect` (:331) — rapid prev/next navigation can render the wrong day/month.
6. Un-awaited/un-caught promise paths: expense delete handler (:3333), `refreshPanel`
   dispatch (:4126) — errors bypass the `announce()` convention.
- Check: jsdom harness (gui-test) tests: member boot without rejection; chart bars carry
  height; customer filter populated after loadCustomers.

### R5 — GUI duplication and oversized blocks (refactor) · MED → ticket #189

- Balance math re-implemented 4× (`:2229`, `:2363`, inline `:1916-1919`, `:2059-2062`).
- Action-cell/table-render blocks repeated 7× (`:703`, `:1353`, `:1580`, `:1758`, `:2866`,
  `:2979`, `:3135`); local `action()` helper defined twice (`:1904`, `:3118`).
- `fillCustomerSelect` vs `fillParentCustomers` near-duplicates (`:300`/`:1605`);
  month-name array re-declared in `weekShort` (`:970` vs `:585`).
- Fetch waterfalls: `refreshInvoices` → `refreshInvoiceSummary` re-fetches `/invoices` twice
  more (`:1898/:2000/:2012`; filter change fires 3–4 GETs, `:4184-4187`); `refreshSettings`
  chains 6 independent awaits (`:3643-3680`).
- Oversized functions: `startApp` ~377 lines (`:4130-4506`), `refreshWeek` ~177,
  `refreshInvoices` ~102.
- Dead/misfire items: empty loop body `:2340-2342`; `void rate` `:2722-2723`;
  `state.invoices` written never read (`:277/:1901`); `fillSelect` third arg ignored
  (`:4575` vs `:118`); double error report in `removeProject`/`removeTask`; `startEdit`
  double-focus (`:845/:847`).
- Backend analogues: `sync_invoice` 195 lines with 5 copied `SyncAttempt` literals
  (`api/invoicing.rs:825-1019`); duplicated cross-ref validation (3 handlers, R3.5);
  `sanitize_filename` (`store.rs:1657`) vs `sanitize_number` (`api/invoicing.rs:316`)
  disagree on dots ⇒ `PdfHint.filename` can differ from `Content-Disposition`;
  duplicated "user has history" scan (`api/people.rs:230-255` vs `store.rs:572-590`).
- Behaviour-preserving; regression = existing suites stay green + no-contract-change check.

### R6 — Comment/doc drift in code (maintainability) · LOW-MED → ticket #190

- `store.rs:1196-1198` `get_entry` doc claims a `MAX_RANGE_DAYS` bound that the body doesn't
  implement (:1204 walks all day dirs).
- `store.rs:1256-1258` `list_all_entries` doc says "not for HTTP handlers" while
  `api/entries_reports.rs:310` calls it per request.
- `domain.rs:785-799` `pdf` doc block attached to `tax_hundredths`; `due_date` doc stale
  (net-14 vs `PaymentTerms` resolution, `api/invoicing.rs:1102-1122`).
- `lock.rs:4-5` claims claims (#24) are a lock provider; claim locking is a hand-rolled scan
  (`api/expenses.rs:165-173`).
- `api/invoicing.rs:241-242` "same clock" comment vs three `app.clock.now()` calls
  (:251/:256/:263).
- Dead task plumbing since #177: `generate_invoice` builds/resolves a task only to pass it to
  `effective_rates(_task)` (`domain.rs:1304-1344,1458`); `build_draft` and
  `api/entries_reports.rs:241-252` still pay `list_tasks` scans for an ignored value.
- Float/overflow hygiene: non-saturating `u64` sums on persisted values
  (`domain.rs:1399,697,911`, `api/expenses.rs:334`, `report.rs:234`); production `expect()`s
  (F9 of backend review) — keep as structured errors.
- `api/vault_config.rs:17` returns the data-dir **path** in `GET /admin/config` — violates
  "responses never expose paths"; `sync_status` echoes raw provider error text
  (`api/invoicing.rs:1022-1029`, `accounting.rs:281-297`).

### R7 — Vanilla/Ubuntu adoption backlog (GUI plan) · MED → ticket #191

Audited against vanillaframework.io/docs & design.ubuntu.com per #181's foundation:

- The vendor build (`gui-test/ubuntu-components.scss`) includes **icons + buttons only**;
  `components.css` has no `p-tabs`, `p-modal`, `p-notification`, `p-card`, `p-chip`,
  `p-table`, `p-badge`. Bespoke equivalents exist: sidebar tabs (`styles.css:101-108`),
  `.seg` (`:355-365`, `:591-593`), `.card` (`:182`), `.chip` (`:213`), `.badge` (`:194`),
  `.notif` (`:230`), nine hand-styled `<dialog>`s (`:167-169`, `:295`, `:304-315`, `:340`).
  Staged plan: (1) extend the pinned scss build with `tables`, `cards`, `modals`,
  `notifications`, `chip`, `badge` (no global reset — the editable timesheet grid is
  app-owned); (2) swap app classes component-by-component behind existing ids; (3) re-run
  contrast + jsdom suites per stage. Do **not** reintroduce GOV.UK styling (#175).
- Fragile icon mapping: `decorateUbuntuButton` matches button **text**
  (`app.js:99-104`) — rename-safe via `data-icon` attributes instead.
- Accessibility fixes: stray non-tab button inside `role="tablist"` (`index.html:65` inside
  :43-66); chart sr-only summary inside `role="img"` is unreachable to AT
  (`index.html:386`, `app.js:2333`); contact-editor and invoice-line inputs labelled only by
  placeholder (`app.js:1409-1413`, `:2468-2484`); `.note-empty` opacity 0.55 on an
  interactive control (`styles.css:279`).
- Automated contrast passes ≠ blanket WCAG conformance; manual keyboard/AT pass on affected
  workflows required per stage.

### R8 — Docs/config accuracy and consolidation · LOW-MED → ticket #192

- `README.md`: example uses image `tucano-time` (:99-102) but every build names the image
  `tucanotime`; auth section (:94-96) still describes pre-#94 ephemeral session keys and
  prod-only Secure cookies (contradicts the README's own persistence table :48-49 and
  `main.rs:123-140`); missing `TUCANO_SECURE_COOKIES`/`TUCANO_PORT`/`TUCANO_MAX_DOCS`;
  storage block (:123-127) thinner than AGENTS.md (duplicated, drifting); Features list
  omits shipped surfaces (timer, calendar/SSO, notifications, recurring, payments).
- `AGENTS.md`: #19 auth paragraph stale (same as README); #8 wording "issued" should be
  "open (issued/paid)" per `lock.rs:85`; storage list missing `config.json`, `session.key`,
  `.server.lock`, `invoices/<id>.pdf`, `.seq.json`, `*.idx.*`.
- `CODE_REVIEW.md`: disposition complete except C3 (`ProjectDoc` **and** `Project`'s unused
  `Deserialize` still coexist — `domain.rs:493/:515`); stray unclosed code fence at :303.
  Historical record — update disposition, don't delete.
- `adr-001`: seam table lists a `NotificationSender` port that no longer exists (removed in
  #101); update row to the current `store.push_notification` seam.
- `gui-test`: README says `npm test` but package `test` runs only `gui.mjs`; CI runs
  contrast + `node --test` suites + `gui.mjs` — align the script.
- `.github/dependabot.yml`: no npm entry ⇒ gui-test deps (jsdom, sass, vanilla-framework,
  css-tree) never get updates though CI gates on them.
- `.gitleaks.toml` is the auto-generated upstream default **with** hand-edited repo
  allowlist entries (:52-57) — regenerate-safe split (`extends` overlay) needed.
- Backup recipe duplicated in README, compose comments and compose labels, and none mention
  that the profiled `tucanotime-cli` service needs `--profile cli`.

## 5. Prior history check (no regressions into fixed items)

All A1–A12/B1–B10 fixes from `CODE_REVIEW.md` (PRs #96–#99) and follow-ups #100–#102 were
spot-verified still in place: fork-PR guards in CI, `verify_slice` constant-time compares,
`.seq.json` invoice numbering, audit rotation, side indexes, `el()`/`fillSelect`, `<dialog>`
helpers (no `window.confirm/prompt` left), vendored swagger, `no-cache` on assets,
`spawn_blocking` scheduler, single-instance lock, generic `Entity` store +
`providers::resolve_secret` dedupe.

## 6. Definition-of-Done mapping for #183

- File inventory/disposition: §3 ✔
- Evidence-backed findings + priorities: §4 ✔
- Language map decisions: §2 ✔
- Vanilla/Ubuntu staged plan: R7 ✔ (stage 1 + a11y fixes shipped; stages 2–3 backlog on #191)
- Implementation of approved refactors: **all landed** — see §7.

## 7. Outcome (2026-10-07)

Every finding R1–R8 was implemented, reviewed and merged; all findings tickets
(#185–#192) and the parent #183 closed with results recorded on each.

| Finding | Ticket | PR(s) | Landed |
| --- | --- | --- | --- |
| R1 fail-open storage/locking | #185 | #194 | ✔ (503 `lock_unavailable`, delete-customer safety, webhook dedup under lock) |
| R2 cross-currency report totals | #186 | #196 | ✔ (per-currency rows; `revenue_by_currency`; QBO string encoding) |
| R3 request-validation gaps | #187 | #197 | ✔ (`InvoiceLineInput`, shared bounds, claim dedup, budget caps, typed retainer errors) |
| R4 GUI boot/async bugs | #188 | #195 | ✔ (chart `style`, member boot, live filter, cache + sequence guards) |
| R5 duplication/size refactor | #189 | #200 + #210 | ✔ (+ startApp `wire*()` split, invoice-summary payload reuse) |
| R6 comment drift + leakage + sums | #190 | #198 | ✔ (path/error leakage, single-clock issue, saturating sums, doc drift) |
| R7 Vanilla/Ubuntu adoption | #191 | #201, #209 | ✔ stage 1 + a11y + `data-icon` contract; stages 2–3 remain as the documented plan in `docs/ubuntu-design.md` |
| R8 docs/config accuracy | #192 | #199 | ✔ (README/AGENTS/CODE_REVIEW/ADR truthed; CI `test:jsdom`; dependabot npm; gitleaks decision documented) |

Post-merge state: `main` CI green (lint-test/build/actionlint/secret-scan/
supply-chain + gui-test job: jsdom suites, contrast, live harness); the review
runtime recorded honestly in §1. Remaining risks recorded on the tickets:
stage 2–3 component swaps need per-stage manual keyboard/AT passes, and the
claims-as-lock-provider strengthening stays a documented follow-up in
`src/lock.rs`.

---

## 8. Round 2 — 2026-10-07, post-programme baseline `07d72d3`

Second full review after every round-1 PR merged (#193–#213 + the dependabot
series). Same rules: verify before claiming; bugs separate from refactors; no
invariant weakening.

**Method.** Cross-boundary seams got the most attention — the places where
independently-reviewed PRs meet (lock semantics vs GUI previews, docs vs code,
dependabot crypto migrations, #182's new async code). Automated checks re-run:
routes ↔ `openapi.json` (0 missing both directions), no `innerHTML`
(3 mentions, all comments), constant-time compares survived the hmac 0.13
migration (`verify_slice` ×3), vendored hashes + rebuild determinism,
main CI 6/6 success, gitleaks/actionlint/cargo-deny green.

**Positive (worth recording, not action):** the #185 fail-closed seam, the #187
validation strictness, the #186 currency partitioning and the #190 leakage fixes
all hold up at their seams; the template-preview `from_timestamp(0,0).unwrap()`
sits on a provably-valid epoch constant; `finish_timer`'s *failure-retry* path
is genuinely safe.

### Findings → tickets

| # | Finding | Severity | Ticket |
| --- | --- | --- | --- |
| R2-1 | `finish_timer` writes the entry before checking the timer exists; two concurrent `POST /timer/stop` calls double-log time (both `get_timer` reads succeed unlocked at `api/timer_calendar.rs:72`) | HIGH (data) | **#214** |
| R2-2 | Committed `Cargo.lock` is not the resolver's output: `cargo build --locked` fails on main; a plain check rewrites 101 lines incl. a base64ct bump — release-image reproducibility is illusory. Cause: `-X theirs` lock resolutions during the dependabot merge queue | P1 | **#215** |
| R2-3 | AGENTS.md (merged #192) says locking covers "issued, partly paid **or paid**"; `is_open()` = `Issued \| PartlyPaid` — paid is *excluded*. `lock.rs:80-82` header still says "issued". Docs claim an invariant the code deliberately doesn't have; the release-on-settle lifecycle needs stating as a decision | MED (docs/architecture) | **#216** |
| R2-4 | `weekLockIds` (web/app.js:992) previews locks from `issued` only; server locks `issued \| partly_paid` → entries on partly-paid invoices look editable, save dies with 409. The correct predicate already exists three times in the dashboard code — one shared helper to rule them | MED (UX correctness) | **#217** |
| R2-5 | Lock verify/write split across two lock acquisitions (entry update/delete, expense delete) lets an issue race through; `create_submission` re-scans invoices+submissions **per entry** (O(N×M) after #185 made every check a full read). Fix: verify inside the store transaction; one snapshot per request | MED (race + perf) | **#218** |
| R2-6 | #182 follow-through: `saveProject` re-scopes the filter without resyncing `#proj-filter-customer` (control lies), the new `refreshProjectTable`/`refreshTaskTable` lack the #188 stale-response guards, and ~20 dead CSS selectors from the old workspace remain | LOW-MED | **#219** |
| R2-7 | Deferred hygiene batch: five invariant `expect()`/`unwrap()` sites, `pay_invoice`'s `starts_with("payment of ")` status branch, duplicated `ensure_admin` inline checks, and round-1's open item C3 (`ProjectDoc` vs `Project::Deserialize`) | LOW | **#220** |

Not everything is new: R2-3/R2-5/R2-7 are explicitly the "recorded
follow-ups" from #190/#192's close comments, promoted to tickets with
evidence; R2-1/2/4/6 are first-observations of the merged code.

### Deliberate non-findings (checked, clean)

- `UpdateError<E>` plumbing (#187): both callers map typed errors; no
  substring parsing survives anywhere (`grep` for `.contains("` on error
  strings: none in api/).
- `revenue_by_currency` + per-currency profitability (#186): rows partition at
  every level; status counts are unit counts — safe cross-currency.
- Vault migration to `Nonce::generate()` (#205): OS CSPRNG via the
  `getrandom` feature chain; on-disk blob layout unchanged.
- jsdom 30 + contrast (CI gates) pass on the merged web/ tree.
