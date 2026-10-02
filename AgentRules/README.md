# AgentRules

Organisation-wide rules for AI agents in TucanoTechnology repositories, synced from
[TucanoAgentRules](https://github.com/TucanoTechnology/TucanoAgentRules). **Do not edit files
here**: the next sync overwrites them — change them in the source repository instead. Rules
specific to one repository belong in that repository's own `AGENTS.md`.

## How to use these rules

Each rule file opens with a `load-when` line naming the work it governs. Read the files that match
the task in hand, plus any file one of them points you to — most tasks need one or two. Do not
read the whole directory.

<!-- routing-table:begin -->
| Rule file | Read when |
| --- | --- |
| [`generic/principles.md`](generic/principles.md) | always |
| [`coding/branching-and-git.md`](coding/branching-and-git.md) | branching, committing, merging |
| [`coding/api-and-data-contracts.md`](coding/api-and-data-contracts.md) | endpoints, payloads, stored shapes |
| [`coding/code-review.md`](coding/code-review.md) | reviewing a pull request |
| [`coding/dependencies.md`](coding/dependencies.md) | adding or upgrading a dependency |
| [`security/security.md`](security/security.md) | secrets, security-sensitive code |
| [`project-management/workflow.md`](project-management/workflow.md) | tickets, raising a pull request |
| [`project-management/ticket-template.md`](project-management/ticket-template.md) | writing a ticket |
| [`test/unit.md`](test/unit.md) | unit tests |
| [`test/contract.md`](test/contract.md) | contract tests |
| [`test/performance.md`](test/performance.md) | performance tests |
| [`test/ui.md`](test/ui.md) | browser or end-to-end tests |
<!-- routing-table:end -->

## Exceptions

Record the exception and its reason in the pull request description, and settle conflicts with the
priority order in [`generic/principles.md`](generic/principles.md); ask the maintainer when that
does not resolve it.
