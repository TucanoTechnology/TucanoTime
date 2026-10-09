# Ubuntu and Vanilla Design Review (#181)

## Findings

The existing UI used Ubuntu colour names but not the Ubuntu typeface. Small
secondary text, rounded controls, inconsistent glyph/emoji icons and similarly
prominent actions made it feel less like a Canonical operational application.
Forms and dense tables needed stronger focus and boundary affordances, and
larger typography exposed narrow-screen overflow in some registers.

## Decision

Use real, selected components from Canonical's Vanilla Framework 4.59.0:
button variants, theme variables, official icons and Ubuntu variable font faces.
Keep the application-owned navigation, native dialogs and editable timesheet
grid. A complete Vanilla reset would unnecessarily change specialised layouts
and require broader workflow migration; no React rewrite is needed.

Generated CSS, font files, licences, asset hashes and corresponding framework
source are in `web/vendor/ubuntu/`. All runtime assets are served by the binary;
no CDN or Node process is needed to build/run the Rust application. Font display
uses swap. The font retains its full width axis; numbers use tabular alignment
where values are compared. The existing colour-mode preference drives both
application tokens and Vanilla's `is-light`/`is-dark` component themes.

To regenerate assets, run `npm ci` in `gui-test`, then `npm run build:ubuntu`.
Build tooling is pinned in its package lock. Sass compiles only the mixins in
`ubuntu-components.scss`; the generator parses CSS URLs, downloads the official
fonts, rewrites them to local paths and records hashes. It includes the original
Vanilla npm source archive, LGPL notice and accompanying GPLv3 text; Ubuntu fonts retain their font
licence. Local application adapters are not Canonical branding or endorsement.

## Staged whole-GUI adoption (#191)

`#183` reviewed the remaining bespoke controls against vanillaframework.io/docs
and design.ubuntu.com. Since #181 the generated bundle carried icons and buttons
only; **#191 stage 1** extended it with the official `p-table` (base + sortable +
mobile-card), `p-card`, `p-chip`, `p-badge`, `p-notification`, `p-modal` and
`p-tabs` patterns. They are compiled in but initially **additive**: no app
template uses them yet, so current screens render unchanged (verified — the
contrast and jsdom suites pass on the rebuilt bundle).

The rules the plan holds to: no global reset (the editable timesheet grid is
app-owned and must keep its dense cell geometry), the rejected GOV.UK direction
(#175) stays rejected, and automated contrast coverage is never presented as
blanket WCAG conformance.

Backlog — one component family per reviewed PR, behind existing element ids,
each gated by the jsdom + contrast suites and a manual keyboard/AT pass on the
affected screens. **Page-by-page execution order, review URLs and ground rules
moved to [`ubuntu-migration-plan.md`](ubuntu-migration-plan.md)** after the API
moved under `/api` and every view gained a stable path (`spa_page` in
`src/lib.rs` ↔ `TAB_ROUTES` in `web/app.js` — keep them in sync):

0. **Stage 2b (in progress)** — cards: the **first-admin sign-in** and the
   **first-run setup wizard** are now twins built on Vanilla `p-card`
   (`p-card__header` + `p-card__content`) with the ruled header, `.65rem`
   field rhythm and full-width `p-button--positive` action (#229). The
   wizard's native dialog keeps its modal behaviour and backdrop but is left
   chromeless so the inner `p-card` is the single frame, matching the auth
   overlay exactly. Its first-customer field is prefilled with the `Tucano
   Time` example. Element ids are unchanged, so the jsdom + wizard smoke
   suites stay the gate. The remaining `.card` sites follow below.
1. **Stage 2a** — tables: the customer/project/task/expense/submission/user
   registers onto `p-table` (invoice register gains `p-table--sortable`;
   dense registers gain the mobile-card pattern for narrow screens).
2. **Stage 2b** — cards/chips/badges: `.card`, `.chip`, `.badge` become
   `p-card`, `p-chip`, `p-badge` adapters.
3. **Stage 2c** — notifications and modal dialogs: the `.notif` list onto
   `p-notification`; the nine bespoke `<dialog>` skins onto `p-modal` while
   keeping the native dialog element (#102 security behaviour unchanged).
4. **Stage 3** — sidebar tabs and segment controls onto `p-tabs`; retire the
   label-matching fallback in `decorateUbuntuButton` once every creation site
   passes `data-icon`/`data-positive` (attribute support shipped with #191).

## Applied Changes

- Ubuntu variable body font, 16px text, restrained heading weights and no
  negative letter spacing or viewport-scaled type.
- Official neutral/positive buttons; Ubuntu orange remains a navigation accent.
- Official navigation, previous/next, play/stop, edit/delete, copy and lock/note
  icons with decorative `aria-hidden` markup and preserved control labels.
- Stronger field boundaries, stable control sizes, clear selected states,
  reduced-motion handling and contained horizontal scrolling for dense tables.
- Existing labelled fields, keyboard tabs, aria-live announcements, native
  dialog focus/escape behavior and exact money calculations remain intact.
- **#191 accessibility fixes**: the setup-wizard button moved out of the
  `role="tablist"` (non-tab children violate required-owned-children); the
  invoice chart's per-month summary moved from a `role="img"` subtree (never
  announced) into the accessible name; contact-editor and invoice-line inputs
  gained real accessible names instead of placeholder-only; faint interactive
  affordances lifted to `opacity .7`; icon/positive styling now honours
  `data-icon`/`data-positive` at creation sites, with the old label/id matching
  kept only as a fallback.

## Accessibility Verification Scope

Automated checks cover palette and real component contrast, local font/asset
integrity, accessible icon names, persistence of themes, dialog security and the
full GUI workflows. Browser checks cover actual font loading, both themes,
desktop/mobile framing, keyboard focus and reflow. This is not a blanket WCAG
conformance claim: full assistive-technology testing and a content-wide error
association audit remain separate work. Dense data grids may scroll within
their container rather than compressing their columns.

## References

- https://design.ubuntu.com/
- https://design.ubuntu.com/accessibility
- https://vanillaframework.io/docs/customising-vanilla
- https://vanillaframework.io/docs/base/typography
- https://vanillaframework.io/docs/patterns/buttons
- https://vanillaframework.io/docs/patterns/icons/accessibility