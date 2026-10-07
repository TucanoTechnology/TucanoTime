# GUI harness (jsdom)

Headless smoke test that loads `web/index.html` + `web/app.js` into jsdom and
drives the real forms against a running server (default `http://localhost:8099`).

```sh
cargo build --release
TUCANO_DATA_DIR=$(mktemp -d) TUCANO_PORT=8099 TUCANO_STRIPE_FAKE=1 ./target/release/tucano-time &
cd gui-test && npm install && npm test
```

`npm test` is the live-server smoke flow only. The full CI gate is
`node contrast.mjs`, the offline jsdom unit suites (`npm run test:jsdom` —
every `*.test.mjs` file) and finally `npm test`, in that order; run all three
locally before opening a GUI PR.

Checks cover: login, day view (#12) rows/week strip/copy-forward, inline add
form, week grid (#13), customers/projects/tasks, reports, invoices, expenses,
submissions, the inline `<dialog>` confirm/prompt flows (#102) and the config
card. Runs in CI as the `gui-test` job (#102); keep it green through any GUI
refactor.

## Visual evidence (screenshots) — #140/#142/#133/#141

The suite itself never compares pixels (deliberately OS-fragile); when a
review needs visual evidence, capture it manually against a local server:

```sh
cargo build
TUCANO_DATA_DIR=/tmp/tt-shots TUCANO_PORT=8080 TUCANO_STRIPE_FAKE=1 ./target/debug/tucano-time
# sign in as the bootstrapped admin in a browser at http://localhost:8080
```

- Desktop (1280px): Timesheets → Calendar segment (#140 month grid), the
  grouped sidebar rail with the 🧾 Invoice / ⏱ Timer shortcuts (#142), and
  Invoices → select a row to open the document preview (#133).
- Narrow (720px, devtools device width): the same three views — rail groups
  wrap horizontally, the calendar collapses columns, the preview card sits
  under the table.
- The #141 E2E workflow check asserts the same surfaces structurally
  (aria-labels, selected states, totals, draft status) — screenshots from
  these steps are its visual companion, not a test input.
