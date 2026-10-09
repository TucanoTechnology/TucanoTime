# Ubuntu/Vanilla rollout — page-by-page migration plan

Goal: standardise the whole GUI on Ubuntu/Vanilla components **without any
big-bang layout change**. Each page is migrated, reviewed live at its own URL,
and signed off before the next one starts. This doc is the tracker;
[`ubuntu-design.md`](ubuntu-design.md) holds the design rationale and the
component backlog.

## Prerequisites (done)

- **Stage 1 (#191)** — Vanilla bundle extended with `p-table`, `p-card`,
  `p-chip`, `p-badge`, `p-notification`, `p-modal`, `p-tabs` (additive).
- **Cards, first pair (#229)** — first-admin sign-in and the setup wizard are
  `p-card` twins.
- **Page routing** — the JSON API moved under `/api`; every view now has a
  stable, case-insensitive URL (`spa_page` in `src/lib.rs`, `TAB_ROUTES` in
  `web/app.js`). This is what makes per-page review-and-sign-off practical.

## Ground rules

1. **One PR per page view**, merged into `develop`; review happens at the
   page's URL before the next page starts.
2. **Layout stays**: component substitution and styling only — no rearranging
   panels, moving controls or rewording flows unless a ticket says so.
3. **Element ids are frozen** (`gui.mjs`, `a11y.test.mjs`, `users.test.mjs`
   etc. drive them). New markup wraps or re-classes, never renames.
4. Each PR ships: the page's Vanilla classes + any shared adapter it needs,
   jsdom/contrast updates, `docs/ubuntu-design.md` note, and a green gate:
   `cargo test --all-targets` + `node --test *.test.mjs` + `node contrast.mjs`
   + `node gui.mjs` (server with `TUCANO_STRIPE_FAKE=1`, as in CI).
5. Both themes are reviewed per page (`.is-dark`/`is-light` tokens) and focus
   rings must stay visible inside `p-card`/`p-modal` overflow.
6. The GUI is pure presentation: no route or contract change is ever needed
   for these PRs — anything touching `src/` or `openapi.json` is a separate
   ticket.

## Page map (review URLs)

| # | Page | URL to review | Status |
|---|------|---------------|--------|
| 0 | Sign-in / first-admin + setup wizard | `/` (pre-auth) | ✅ #229 |
| 1 | Timesheets — Day | `/timesheets/day` | ⬜ |
| 2 | Timesheets — Week | `/timesheets/week` | ⬜ |
| 3 | Timesheets — Calendar | `/timesheets/calendar` | ⬜ |
| 4 | Customers | `/customers` | ⬜ |
| 5 | Projects | `/projects` | ⬜ |
| 6 | Tasks | `/tasks` | ⬜ |
| 7 | Invoices | `/invoices` | ⬜ |
| 8 | Expenses | `/expenses` | ⬜ |
| 9 | Approvals | `/approvals` | ⬜ |
| 10 | Reports | `/reports` | ⬜ |
| 11 | Settings — Security & runtime | `/settings/security` | ⬜ |
| 12 | Settings — Users | `/settings/users` | ⬜ |
| 13 | Settings — Invoice documents | `/settings/invoices` | ⬜ |
| 14 | Settings — Products & services | `/settings/catalog` | ⬜ |
| 15 | Settings — Expense categories | `/settings/expenses` | ⬜ |

Cross-navigation (row "Projects" buttons, wizard finish, calendar → day) is
expected to keep working through the tabs' click handlers; a follow-up ticket
may turn those into real links once every page is migrated.

## Order and per-page scope

The sequence deliberately front-loads the shared adapters (Day carries the
first `p-table` register and the entry form; Invoices carries the most
patterns). Each page lists what "migrated" means — nothing layout-level.

1. **Timesheets — Day.** `.day-bar` already uses `p-card`; migrate the entry
   form to `p-field`-style rhythm, the register to `p-table`, badges to
   `p-badge`, copy-forward bar to the shared card adapter. Deliver: the
   `p-table` adapter used by every later register.
2. **Timesheets — Week.** Grid keeps its app-owned geometry (explicitly out of
   scope per `ubuntu-design.md`); migrate the surrounding controls
   (segmented Day|Week|Calendar to `p-tabs`, week totals footer affordances).
3. **Timesheets — Calendar.** Month navigation cluster + legend onto shared
   button/card adapters; grid cells stay app-owned.
4. **Customers.** Register onto the Day `p-table` adapter; create/edit popup
   onto the `p-modal` skin; contact rows onto form rhythm.
5. **Projects.** Same adapters; scope-label + filter row onto `p-field`.
6. **Tasks.** Same; hierarchy filters onto the shared filter-field pattern.
7. **Invoices.** Biggest page: Open/All segment to `p-tabs`, register gains
   `p-table--sortable`, preview panel + tracked-work wizard dialogs onto the
   modal/card adapters, payment dialogs included.
8. **Expenses.** Register + quick-add category popup reuse earlier adapters.
9. **Approvals.** Submission table + decision dialogs reuse earlier adapters.
10. **Reports.** Query bar onto `p-field`, result table onto `p-table`,
    CSV link as `p-button--link` pattern.
11. **Settings — Security & runtime.** Config table + theme switch onto
    form/button adapters.
12. **Settings — Users.** User register + create/reset-password dialogs.
13. **Settings — Invoice documents.** Template editor + preview onto card
    adapters.
14. **Settings — Products & services.** Item catalog table + dialog.
15. **Settings — Expense categories.** Table + dialog; final cleanup PR: retire
    the bespoke `.card`/`.bar`/`.field` skins that have no remaining users and
    shrink `web/styles.css` (split into its own PR when it exceeds a page's
    worth of churn).

## Per-page review workflow

For page *N*: implement → open PR → **hold merge** until you review
`http://<host>:8080/<url>` (or the PR's dev server) in **both themes**, check
keyboard/AT basics, then approve → merge → tick the table above → next page.
Referencing a page in chat is just its URL, e.g. *"review
`/timesheets/week`"* — it deep-links from a fresh session after login.

## Tracking

Suggested: one GitHub issue per row (title `Vanilla rollout: <page>`, label
`ui`, blocked-by the routing foundation issue), mirroring the table above;
this doc stays the index.
