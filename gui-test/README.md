# GUI harness (jsdom)

Headless smoke test that loads `web/index.html` + `web/app.js` into jsdom and
drives the real forms against a running server (default `http://localhost:8099`).

```sh
cargo build --release
TUCANO_DATA_DIR=$(mktemp -d) TUCANO_PORT=8099 TUCANO_STRIPE_FAKE=1 ./target/release/tucano-time &
cd gui-test && npm install && npm test
```

Checks cover: login, day view (#12) rows/week strip/copy-forward, inline add
form, week grid (#13), customers/projects/tasks, reports, invoices, expenses,
submissions, the inline `<dialog>` confirm/prompt flows (#102) and the config
card. Runs in CI as the `gui-test` job (#102); keep it green through any GUI
refactor.
