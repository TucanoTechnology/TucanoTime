---
load-when: adding or changing an HTTP endpoint, request, response, or persisted data shape
applies-to: repositories that expose an HTTP API or persist structured data
title: API & data contracts
---

# API and Data Contracts

## Endpoints

- Model each resource as RESTful endpoints.
- Every endpoint, parameter, request, response, and error must be represented in the repository's
  machine-readable API contract and be usable through its documentation UI. If it is not in the
  contract, it does not exist.
- Return consistent structured JSON error responses.

## Validation

- Validate every external payload against the checked-in schema or contract **before** business
  logic or persistence, including all create and update operations (`POST`, `PUT`, `PATCH`).
  Client-side validation alone does not satisfy this.
- Reject malformed JSON, wrong types, missing required fields, unknown fields, and oversized or
  schema-incompatible values.
- Do not partially persist invalid input — validation completes before any write or side effect.
- On failure, return a consistent client error naming the affected fields without exposing
  internals, secrets, paths, or stack traces.
- Version schemas with the code that consumes them, and document intentional breaking changes.
  Adding a field to a closed schema is breaking and needs an explicit versioning plan first.

Filenames and identifiers reaching the filesystem: [`../security/security.md`](../security/security.md).
Test coverage for these rules: [`../test/contract.md`](../test/contract.md).
