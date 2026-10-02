---
load-when: creating, updating, or closing a ticket, or raising a pull request
applies-to: every repository
title: Ticket workflow
---

# Workflow

## Tickets

Every change has a ticket before implementation begins — if none exists, create one first. Use
[`ticket-template.md`](ticket-template.md), and state the goal, scope, and expected outcome.

- Keep the title concise; the description is the single source of truth for scope and intent.
- Comment as you work: progress, decisions, blockers, deviations from the plan, open questions.
  Do not wait until the end to communicate a change.
- Keep labels, priority, and links to related tickets and pull requests current, and revise a
  ticket that no longer reflects the task rather than leaving it stale.
- Close only once the work is verified against the definition of done and merged: add a final
  summary comment, reference the pull request and merged commits on the ticket, reference the
  ticket number in the pull request description and commit messages, and move the item to Done on
  the project board.

## CI/CD validation

All CI/CD jobs must pass locally before committing and raising a pull request: run the
repository's lint and formatting, test, and build commands first, and fix what they report. If a
job fails after pushing, fix it before starting other work. The commands themselves live in that
repository's `AGENTS.md`.

## Pull request dependencies

When one pull request needs another merged first:

- Record it in the title — `[Depends on #XX]` or `[Blocked by #XX]` — and explain why in the
  description.
- Merge in dependency order: API changes before the clients that use them, foundational features
  before dependents, breaking changes before migrations.
- Rebase on the target branch before the final merge.
