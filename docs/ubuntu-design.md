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