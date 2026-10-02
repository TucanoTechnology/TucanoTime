---
load-when: reviewing a pull request, or preparing one for review
applies-to: every repository
title: Code review
---

# Code Review

Review for correctness, security, maintainability, test coverage, readability, performance, error
handling, and consistency with these rules and the repository's `AGENTS.md`. Ask for
clarification when the intent is unclear rather than assuming it.

As an author, keep the pull request small, scoped to one logical change, and state its intent,
tradeoffs, and known limitations in the description. Tests are required for behaviour changes.

Report the impact of the change, and separate must-fix issues from suggestions.

## Block approval when

- Behaviour changed without a meaningful test.
- Security or data handling is unclear, or a secret may be exposed.
- The change regresses existing behaviour or an accessibility gate.
- A shared rule is broken and the exception is undocumented.
- The implementation cannot be understood or justified.

Approve once the change is safe, tested, and understandable.
