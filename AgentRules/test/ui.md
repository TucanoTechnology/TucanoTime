---
load-when: writing or changing end-to-end or UI tests
applies-to: repositories with a user interface
title: Browser & end-to-end tests
---

# UI Tests

Arrange the required data and state; act by navigating and interacting with the interface; assert
the visible result.

- Cover user-visible journeys and behaviour, not internal implementation details.
- Prefer stable selectors such as `data-testid`; reuse helpers for common flows like login and
  navigation.
- Wait for a specific condition instead of using fixed sleeps or timing assumptions.
- Keep tests isolated and reset state between runs.
- Keep each assertion focused on one observable outcome.
- Attach screenshots or traces to failures so they can be debugged without a rerun.
