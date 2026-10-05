# Code review — TucanoTime (2026-10-05)

Full-system review after the integration burst. Every line reference was re-read
and verified; two sub-review claims I could **not** reproduce are called out at the
bottom so nobody chases them.

Priorities: **SECURITY → CORRECTNESS → MAINTAINABILITY → EFFICIENCY**. Items are
ordered by blast-radius within each. `Effort` is rough sizing for planning.

---

## A. SECURITY

### A1 — CI runs fork-PR code on self-hosted runners · HIGH · Effort: S
`.github/workflows/ci.yml:4` (`pull_request: {}`) + `:17,34,44,56,69`
(`runs-on: [self-hosted, …]`). Every job checks out and `cargo test`/`docker build`s
untrusted fork code on org-owned infra with a live `GITHUB_TOKEN`. That's runner
compromise / token theft, not a sandboxed CI failure.
**Fix:** gate to trusted events — `pull_request` from the base repo only
(`if: github.event.pull_request.head.repo.fork == false`) plus a
`workflow_run`/label-gated path for forks, or move the untrusted build to a hosted
runner. Keep top-level `permissions: contents: read`.

### A2 — SAML adapter "verifies" with HMAC keyed on the **public** cert fingerprint · HIGH · Effort: M
`src/sso.rs:219` `sign(&self.cert_fingerprint, statement)`; `verify` (`:239`) only
checks that HMAC. A fingerprint is a hash of the IdP's *public* certificate — anyone
holding the IdP metadata can forge an assertion and log in as any email; with the
empty default allow-list (`registry_from_vault`) that is arbitrary-account login, and
`map_role` can make it Admin.
**Fix:** this is a stub, but it is *routable*. Either (a) gate `SamlIdp` behind a
real XML-DSig verifier before any SAML provider is registered, or (b) at minimum mix
a deployment secret into the HMAC (`HMAC(fingerprint ‖ TUCANO_SAML_KEY, …)`) and
default `allowed_domains` to non-empty / fail-closed. Never register a provider whose
`verify` can be satisfied from public data.

### A3 — Webhook/demo flags are config.json-writable, and `fake` disables all verification · HIGH · Effort: S
`src/appconfig.rs:124,130` put `stripe_demo`/`paypal_demo` in the whitelist
(`PUT /admin/config` persists them); `src/payments.rs:200-202` skips HMAC entirely
when `fake` is set. A config-only flip turns off webhook auth for a live provider →
unsigned bodies settle invoices. **Fix:** drop `*_demo` from the config whitelist
(env-only, like real secrets), and refuse to boot with `fake=true` when a real
`*.webhook_secret` exists or `TUCANO_ENV=production`.

### A4 — Profitability & budget reports leak cost data to every member · HIGH · Effort: S
`src/lib.rs:112-113` route `/reports/profitability` and `/reports/budgets` in the
**protected** tier; `api.rs` `profitability`/`budget_report` apply no
`visible_to`/`ensure_admin`. Members see per-person cost rates, margins and budgets.
Contradicts the #51 privacy model. **Fix:** move both to the admin router (one line
each); add a member-403 contract test.

### A5 — Default compose deployment can't log in over plain HTTP · HIGH · Effort: S
`docker-compose.yml:28` defaults `TUCANO_ENV=production` → `Session::secure=true` →
`; Secure` cookie (`src/auth.rs:165-169`). The app serves plain HTTP on :8080, so a
LAN/IP origin drops the cookie and login loops. The operator's workaround (clear
`TUCANO_ENV`) also drops the strong-secret enforcement. **Fix:** decide the Secure
flag from scheme (`TUCANO_SECURE_COOKIES` or `X-Forwarded-Proto` behind TLS), not from
the prod/secret axis; document "embedded LAN HTTP ⇒ no Secure flag" as expected.

### A6 — Real SSO POST binding is blocked by the CSRF guard · MED · Effort: S
`src/lib.rs:277-280` exempts login/bootstrap/webhook but **not** `/auth/sso/assertion`
(`lib.rs:62`). A genuine IdP form-POST can't send `X-CSRF-Protection: 1`, so SSO 403s
the moment a real adapter lands. **Fix:** add `/auth/sso/assertion` to the exempt set
(pre-session, authenticated by the assertion) + a no-CSRF-header contract test.

### A7 — Signature digests compared with a short-circuiting loop · MED · Effort: S
`src/payments.rs:186` `got.bytes().zip(want.bytes()).all(|(a,b)| a==b)` (comment even
says "Constant-ish"); same shape in `sso.rs`. `auth.rs:149` already does it right.
**Fix:** one `hmac::Mac::verify_slice` helper used by payments + sso; delete the
byte loops. (Practical timing risk is low for HMAC, but this is the cheap, correct
primitive.)

### A8 — `hint()` leaks the last 4 chars of every secret · LOW · Effort: S
`src/vault.rs:169` `format!("••••{}", &v[v.len()-4..])`, shown by `list_secrets`
(`api.rs:619`). Admin-only, but more than "names only". **Fix:** last-2 or just
"set · <date>".

### A9 — Swagger UI from unpkg, origin in CSP, no SRI · MED · Effort: S
`web/swagger.html:7,10` + `lib.rs:252` `script-src 'self' https://unpkg.com`. A
compromised/mutable CDN tag executes script in the **admin origin** (session cookie
in scope) and breaks offline. **Fix:** vendor `swagger-ui-bundle.js` under `web/`
(rust-embed serves it), drop `unpkg.com` from CSP.

### A10 — No `Cache-Control` on embedded assets · MED · Effort: S
`src/lib.rs:325-329` sends only `Content-Type`; same `/app.js`,`/styles.css` URLs
across binary swaps ⇒ browsers serve a stale GUI against a new API after an update.
**Fix:** `Cache-Control: no-cache` on html/js/css, or content-hashed names + `immutable`.

### A11 — Webhook ignores amount/currency · MED · Effort: M
`src/payments.rs` `WebhookPayment` carries only reference+number; `api.rs:1080-1083`
settles the invoice regardless of what was collected. **Fix:** carry
`amount_minor`/`currency` in the event, compare to the invoice, else record a partial.
(Signature is correctly checked *before* any state change — verified good.)

### A12 — Misc security hygiene · LOW
- `bootstrap` check-then-create is not atomic (`api.rs:339`→`361`, `store.rs:208`) —
  two first requests can both mint an admin. `Store::create_first_user` under one lock.
- `/auth/sso/assertion` has **no rate limit** (`api.rs:473+`) where login does; add a
  per-`email`/`key` bucket.
- `ratelimit.rs:48` `record_failure` never evicts ⇒ unbounded map from random-email
  floods. Prune expired windows + size cap.
- Internal strings leak to clients: `api.rs:488,1032,1065` `other.to_string()`;
  persisted `record.error` (`api.rs:1170/1202/1237`) is echoed by `sync_status`.
  `tracing::warn!` the detail, return a generic message.

---

## B. CORRECTNESS / DATA-INTEGRITY

### B1 — Budget alerts are silently dead · HIGH · Effort: S
`src/budgets.rs:124` `from = today - 3650 days`, but `store.rs:28` `MAX_RANGE_DAYS=400`
⇒ `list_range` always returns `RangeTooLarge`, swallowed by `unwrap_or_default()`
(`:125`) ⇒ entries always empty ⇒ burn 0 ⇒ the 80/100% alert never fires. **Fix:**
a `list_all_entries()` job path (or clamp to `MAX_RANGE_DAYS`); never `unwrap_or_default`
a load-bearing scan. This is the single most impactful bug found.

### B2 — Recurring job can double-bill *and* skip a period · HIGH · Effort: M
`src/recurring.rs:138-142`: `create_invoice` (lock #1) then `put_schedule` (lock #2),
both `let _ =`. If the invoice lands but the schedule write times out, next run mints a
**second** draft for the same period (drafts aren't in the `excluded_*` set — only
*Issued* are, `recurring.rs:82-95`). If invoice generation errors, the schedule still
advances ⇒ a billing period is silently skipped. **Fix:** one `Store` txn taking a
single `write_lock` that writes the invoice *and* advances the schedule; advance only
on success; log on failure.

### B3 — Invoice numbers are recycled after a delete · HIGH · Effort: S
`store.rs:287,296` number = `list_invoices().len()+1`; `delete_invoice` is exposed
(`api.rs:1286`). Deleting INV-0003 makes the next create re-mint INV-0003;
`find_invoice_by_number` (earliest-wins) can then settle the *wrong* invoice on a
webhook. **Fix:** monotonic counter file (`.seq.json`) minted inside the same
`write_lock` as `create_invoice`; make `next_invoice_number` private.

### B4 — Multi-step writes split across locks with swallowed errors · HIGH · Effort: M
Same root cause as B2 — read-check-write with no single lock:
- `api.rs:2413-2415` entry day-move: `put_entry` then `let _ = delete_entry(old)` ⇒ a
  timeout leaves the id in two day folders ⇒ every report double-counts it, `get_entry`
  shows a stale shadow.
- `api.rs:1761-1762` `stop_timer`: `put_entry` then `delete_timer?` — a 503 on the
  second lock leaves the timer running ⇒ user retry creates a second entry.
- `issue_invoice` / `update_entry` read lock-state (`lock.rs:77`, unlocked
  `list_invoices`) outside the write lock, so an issue racing an edit is missed.
**Fix:** a `Store::with_write_lock(|txn| …)` transaction seam and route these
multi-writes through it (also fixes A12-bootstrap, B2).

### B5 — Scheduler blocks the tokio worker · MED · Effort: S
`scheduler.rs:68` `tokio::spawn(async { self.tick() })` calls `job.run` **synchronously**
inside the runtime; jobs do thousands of file reads **and blocking network I/O**
(`accounting.rs:285` ureq, `email_reminders.rs:139` SMTP). A slow provider stalls an
async worker (and the HTTP API behind it). **Fix:** wrap `tick` in `spawn_blocking`.

### B6 — Cross-process state divergence (the CLI shares the volume) · MED · Effort: M
`revoke.rs` and `vault.rs` load once at boot and cache in memory; the scheduler keeps
a per-process `last` map. With the documented `tucanotime-cli --backup` (and the
multi-process design `lock.rs` implies): a revocation on A is invisible to B; each
process's `vault.put` rewrites the whole map (last-writer-wins deletes the other's
secrets); two schedulers fire every job ⇒ duplicate reminders/emails, and (with B2)
duplicate invoices. **Fix:** flock a single-instance boot lock; or re-read
revoked/vault under the lock on use. At minimum, document "one server process per
volume".

### B7 — Non-atomic writes outside the store lock · MED · Effort: S
`scheduler.rs:56`, `revoke.rs:48`, `email_reminders.rs:62`, `vault.rs:140` all
`fs::write` (truncate+write) without the tmp+rename discipline the store uses. The
documented backup-on-the-same-volume can capture a half-written `secrets.bin` or
revoke list. **Fix:** route all four through `Store::write_state_file` (tmp+rename).

### B8 — Scheduler marks a job "ran" before running it · LOW · Effort: S
`scheduler.rs:52,56` persist the timestamp before `job.run` (`:60`) ⇒ a crash mid-job
skips that day's reminder/email. **Fix:** persist after the run, or per-job success flag.

### B9 — Xero sync posts sales invoices as bills · LOW · Effort: S
`accounting.rs:234` `"Type": "ACCPAY"` (a *bill*) for a customer invoice. **Fix:**
`ACCREC`. Likely breaks the mock's assumptions too — verify against a recorded fixture.

### B10 — Webhook event matching is too loose · LOW · Effort: S
`payments.rs:207` `event.ends_with(".checkout.completed")` matches any
`*.checkout.completed`; an absent `payment_status` still settles. **Fix:** exact-match
each provider's terminal events.

---

## C. MAINTAINABILITY

### C1 — Split `api.rs` (2,595 lines) · Effort: M
Break along the existing section banners into `api/{auth, vault_config, people,
invoicing, expenses, timer_calendar, entries_reports}.rs` with `api/mod.rs` holding
`AppState`/extractors/guards. Keep `routes()` centralized in `lib.rs`.

### C2 — Extract a generic entity store (kills ~12 copies in `store.rs`) · Effort: M
Each collection repeats `dir_entries → read_json → sort` / `write_lock →
create_dir_all → write_json` / `exists→NotFound→remove_file`. A `trait Entity { const
DIR; fn id(&self) }` + `Collection<T>` ops collapses them and puts the lock/atomicity
rules (needed by B4/B7) in **one** place. Nested project/task collections take a
parent-key + a `decode` hook (where `project_from_bytes` plugs in).

### C3 — Retire the `ProjectDoc` shim or `Project`'s unused `Deserialize` · Effort: S
Two disk read-paths that can drift; `project_from_bytes` has a single caller
(`store.rs:660`).

### C4 — De-duplicate the provider/registry scaffolding · Effort: M
`vault.get(key).or_else(env)` is re-implemented in payments/sso/accounting/calendar_oauth
with **divergent precedence** (SSO does env>config>vault, accounting config>vault>default)
— a correctness smell, not just duplication. One `resolve_secret(vault, cfg, key, env)`
+ one generic `Registry<T>`. `notify.rs::LogNotifier` is a dead seam (jobs call
`store.push_notification` directly) — two notification abstractions, delete one.

### C5 — Handler-shape helpers · Effort: S
`get_invoice_or_404` (6 copies), `Registry::get_or_400` (3 copies), a
`blocking(fn)->Result<_,ApiError>` for the spawn_blocking/JoinError/domain-error
boilerplate (4 copies). Typed `RangeQuery` instead of `Query<HashMap<..>>` + manual
parse (gives free validation).

### C6 — `app.js` structure (2,180 lines, all globals) · Effort: M
Build-free wins: an `el(tag,{cls,text,attrs,on})` + `option()`/`fillSelect()` helper
(~350 lines → ~120 across 7 table renderers + 3 select-fillers); route all server
errors through `showFormError`/`announce` (drop `window.alert` double-report); replace
`window.confirm/prompt` with one `<dialog>`. Real modules (`<script type=module>`) work
under rust-embed + the current CSP.

### C7 — GUI cache/refresh correctness (bugs, not just style) · Effort: S
- `refreshCustomerPickers` rebuilds only `entry-customer`; `invoice/expense/timer`
  pickers are filled once at boot ⇒ a newly-added customer can't be invoiced until
  reload (`app.js:1201` vs `2050`). Build the picker list from one array.
- `refreshPanel` map omits `tab-customers`/`tab-settings` ⇒ stale rows after
  cross-tab/API edits (`app.js:1933`).
- `removeCustomer` doesn't clear `selectedCustomerId`/`projectsByCustomer[cid]` ⇒
  project form points at a deleted customer; cache leak (`app.js:936`).
- `jumpToEntry` fires `refreshDay` twice (via tab click + explicit `await`) and scrolls
  to `tbody tr` (first row), not the entry's — re-entrant refresh guard + `tr.dataset.entryId`.

### C8 — Action/status-code consistency · LOW
Creates 201, but `checkout` 201 vs `stop_timer`/`submit_claim` 200; pick a rule.

---

## D. EFFICIENCY

### D1 — `startApp` is a ~17-request sequential waterfall before first paint · Effort: S
`app.js:2044-2063`: after `loadCustomers()` (a real dependency), `await Promise.all`
the independent panels; three `fillProjectSelect(…, '', …)` calls are awaited but only
render a placeholder — drop them. Cuts first-paint RTTs from ~17 to ~2.

### D2 — Per-save fetch storms · Effort: S
`refreshDay` = day GET + week-strip GET + calendar GET sequentially (`app.js:277,333`);
a single grid-cell commit = GET entry + PUT + `refreshWeek` (which re-fetches
`/submissions` + `/invoices` just to compute lock state, `app.js:511-528`). Parallelize
independent fetches; reuse the day GET for the strip (day ⊂ week); TTL-cache
`weekLockIds` per render; patch the affected `<tr>` total from the PUT response
instead of a full grid re-render.

### D3 — Backend N+1 filesystem fan-out · Effort: M
Reports/billing walk `list_customers → per-customer list_projects (+each re-reads
`get_customer`, `store.rs:631`) → per-project list_tasks → list_range(≤400 day-dirs)`
⇒ ~12-15k `open()` per report at 100×10 scale, repeated per dashboard poll. `find_invoice_by_number`
and `get_user_by_email` full-scan on the **webhook/login** hot paths. **Fix:**
`list_all_projects()/list_all_tasks()`; a small mtime+size cache for the tiny
metadata collections (users/customers/projects/tasks); `number→id` and `email→id`
side-indexes maintained under the write lock. Entries-by-day is fine at this scale —
don't cache those.

### D4 — O(n²) in billing math · Effort: S
`generate_invoice` linear-scans `excluded_entries`/`excluded_expenses` and does
`projects.iter().find`/`tasks.find`/`users.find` per entry (`domain.rs:733-786`);
same in `budgets.rs:55` / `report.rs`. 10⁸ comparisons at 10k×10k. **Fix:**
`HashSet` for exclusions, `BTreeMap`s for project/task/user built once per call.

### D5 — Backup holds the whole tree in memory · Effort: S
`backup.rs` `blobs: Vec<Vec<u8>>` + in-memory tar members ⇒ OOM on a multi-GB data dir.
Stream each file from disk into the gzip sink.

### D6 — Misc · LOW
Audit log has no rotation and `recent()` reads the whole file (`audit.rs:47`);
`dir_entries` counts stray `.json.tmp` orphans toward `TooManyItems` (skip `*.tmp`
and prune on open); reminders do 2N per-user lock round-trips inside one tick.

---

## Sub-review claims I could NOT reproduce (do not chase)
- *"backup `restore()` never sanitizes paths"* — **false**; `backup.rs:175` calls
  `sanitize(&name)?` per entry (and there are traversal tests at `:436-442`). The
  in-memory/streaming concern (D5) and symlink-clobber are still real, but the path
  itself is checked.
- *"`self.store.list_invoices().unwrap()` in the email-reminder job"* — the only
  `unwrap()`s in `email_reminders.rs` are in `#[cfg(test)]` (`:158-201`); production
  uses `match … else { return }`. No change needed.

## Suggested sequencing
1. **B1, A4, A5, A1, B3** — dead feature + privacy leak + broken default login +
   CI supply chain + wrong-invoice-paid. Small, high-blast-radius.
2. **B2 + B4 + B7** via one `Store::with_write_lock` transaction seam (fixes
   double-billing, duplicate entries, torn backup reads together).
3. **A2, A3, A6, A7, B5** — provider/webhook hardening + non-blocking scheduler.
4. **C1, C2, C4** — the two big structural refactors (they make everything above
   safer to change again), then **D1-D4** efficiency, then **C6/C7** GUI.
```
