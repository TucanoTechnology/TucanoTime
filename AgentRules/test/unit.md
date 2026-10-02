---
load-when: writing or changing unit tests
applies-to: every repository
title: Unit tests
---

# Unit Tests

Arrange the state and inputs, act on the code under test, assert the observable outcome. Each
test verifies one thing and fails for one reason.

- Verify one unit of behaviour at a time.
- Mock only what is outside the unit boundary, and assert behaviour rather than implementation.
- Cover success, failure, and edge cases.
- Keep setup minimal and readable, and tests fast enough to run on every change.
- Treat the tests as executable documentation of expected behaviour.
