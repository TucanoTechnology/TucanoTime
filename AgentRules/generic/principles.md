---
load-when: always — read before starting any task
applies-to: every repository
title: Principles
---

# Principles

These priorities decide judgement calls when rules, tickets, or requirements conflict, or when no
rule covers the situation.

- **Security over new features.** A change is not done if it introduces an unresolved security
  risk. Descope or delay a feature rather than ship a known vulnerability, and raise a ticket for
  security work deferred to a later change.
- **Accessibility must not regress.** Do not merge a change that breaks an accessibility
  conformance gate a repository has already achieved. Treat an accessibility regression exactly as
  a failing test.
- **Automate over manual.** Prefer a CI check, script, or workflow to a manual step. Where one
  must remain, document why automation is not yet possible and raise a ticket to close the gap.
