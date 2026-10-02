---
load-when: handling credentials or secrets, or changing anything security-sensitive
applies-to: every repository
title: Security
---

# Security

## Secrets

Never commit API keys and tokens, passwords and secrets, private keys and certificates, database
credentials, or environment-specific configuration.

Keep secrets in environment variables or ignored `.env` files. If a secret is exposed, revoke or
rotate it immediately and report the exposure through the approved security process.

## Protected branches

- Protect the default branch: every change merges through a pull request with at least one
  approving review and all required CI checks passing.
- Keep the pull request branch up to date with its target before merging.
- Never bypass branch protection or a required security check.
- Security-sensitive changes need review by the appropriate maintainer.
- A pull request that exposes a secret, or introduces an unresolved security risk, must not merge.

## Untrusted input

- Sanitise every user-supplied identifier and filename before it reaches the filesystem, and
  prevent path traversal.
- Never return internal paths, stack traces, or raw filesystem errors to a client.
