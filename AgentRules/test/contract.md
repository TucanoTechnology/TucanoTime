---
load-when: writing or changing contract tests, or changing an exchanged payload or schema
applies-to: every repository
title: Contract tests
---

# Contract Tests

Contract tests verify that producers and consumers agree on the structure and behaviour of
exchanged data.

- Test valid payloads against the checked-in schema or API contract.
- Test malformed JSON, invalid types, missing required fields, unknown fields, boundary values,
  and oversized input.
- Verify invalid payloads are rejected before persistence or any other side effect.
- Verify validation errors use the documented status, error shape, and field-level detail, and
  never expose sensitive values or implementation details.
- Cover each supported schema version, and record what compatibility is expected between them.
- Use representative fixtures for normal, boundary, and invalid payloads.
- Keep the tests deterministic and independent of external services.
- Update them whenever a schema or exchanged payload changes, and run them in CI before merging
  such a change.
