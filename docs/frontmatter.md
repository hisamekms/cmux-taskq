---
id: docs-frontmatter
type: design
title: Documentation frontmatter specification
status: current
created: 2026-09-21
updated: 2026-09-26
last_verified: 2026-09-26
tags:
  - documentation
  - conventions
---

# Documentation frontmatter specification

## Common fields

```yaml
---
id: unique-document-id
type: adr
title: Human-readable title
status: accepted
created: 2026-09-21
updated: 2026-09-21
owners:
  - hisamekms
tags:
  - architecture
related:
  - design-overview
---
```

`id`, `type`, `title`, `status`, `created`, and `updated` are required. `owners`, `tags`, and `related` are optional lists of strings. Dates use ISO 8601 calendar dates (`YYYY-MM-DD`). IDs are stable and use lowercase kebab-case, except ADR IDs. An ADR ID is `adr-NNNN` for the four-digit ADRs (0001 and the numbers already reserved by registered tasks) and `adr-t<task ID>-<N>` for new ADRs, where the task ID is that of the task that writes the ADR and `N` is a branch number from 1 (always present, even for a single ADR). A new ADR's filename is `docs/adr/<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md`, dated with its `accepted_on` (a `proposed` ADR uses the date it was written and is renamed to match `accepted_on`, with the links to it, in the change that accepts it). References use `ADR-t<task ID>-<N>` without the date.

`updated` is the last content change. Design documents also use `last_verified` for the date on which the document was checked against the implementation.

## Type-specific fields

| Type | Allowed status | Additional fields |
| --- | --- | --- |
| `adr` | `proposed`, `accepted`, `rejected`, `superseded`, `deprecated` | `accepted_on`, `superseded_by`, `superseded_on`, `deprecated_on`, `supersedes`, `amends`, `amended_by` (see [ADR fields](#adr-fields)) |
| `design` | `draft`, `current`, `deprecated`, `superseded` | `last_verified`, optional `scope` |
| `plan` | `proposed`, `active`, `blocked`, `completed`, `archived` | optional `milestone`, `target`, `depends_on` |

Design documents describe the current state and may be edited. Plans describe intended work and may be edited while active. The progress and state of individual tasks live in the dagq queue, not in documents.

## ADR fields

The rules follow [ADR-t598-1](adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md). Only an `accepted` ADR is a current decision, and every decision in its body is in force except those named by its `amended_by` ADRs.

| Status | Meaning |
| --- | --- |
| `proposed` | Under consideration. Its decisions are not in force yet |
| `accepted` | Adopted. Every decision in its body is in force, except the decisions changed by the ADRs in its `amended_by` |
| `rejected` | Not adopted |
| `superseded` | Replaced as a whole by the ADR in `superseded_by` |
| `deprecated` | Retired without a successor |

| Field | Carried by | Value |
| --- | --- | --- |
| `accepted_on` | `accepted`, `superseded`, `deprecated` | The date the ADR moved from `proposed` to `accepted`. A `rejected` ADR has none |
| `superseded_by` | `superseded` | One ADR ID of the successor. If the successor is itself superseded, the reader follows the chain to an `accepted` ADR |
| `superseded_on` | `superseded` | The date the ADR became `superseded` |
| `deprecated_on` | `deprecated` | The date the ADR became `deprecated` |
| `supersedes` | The replacing ADR | A list of the ADR IDs it replaces |
| `amends` | A new ADR that changes part of a frozen large ADR | A list of `<ADR ID> decision <N>` entries it changes, for example `adr-0047 decision 24` |
| `amended_by` | A frozen large ADR changed in part | A list of the IDs of the ADRs that amend it |

```yaml
status: superseded
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
superseded_by: adr-0040
superseded_on: 2026-09-25
```

```yaml
status: deprecated
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
deprecated_on: 2026-09-25
```

A `deprecated` ADR has no `superseded_by` or `superseded_on`, and a `superseded` ADR has no `deprecated_on`.

- **Small ADRs.** One ADR holds one decision (a few tightly bound ones at most), and its body stays within about 100 lines. It records what needs a person's judgement to change: the problem and context, policy, principles, boundaries and invariants, rejected alternatives, and consequences. Event kinds and payload fields, CLI flag spellings, JSON shapes, default and threshold values, function, module and file names, migration numbers, and test names go to `docs/design/`, which holds the current state; the ADR holds why.
- **Whole replacement.** An ADR that changes even one decision of an existing ADR rewrites and carries over the old ADR's decisions that are still in force, and the old ADR becomes `superseded` as a whole. One ADR may replace several.
- **Amending a frozen large ADR.** The existing ADRs with many decisions (such as ADR-0047, 0044 and 0073) are frozen. To change part of one, write a small new ADR that lists the changed decisions in `amends`, add its ID to the old ADR's `amended_by`, and bring the `docs/design/` documents to the current state in the same change. A small new-form ADR is never amended; it is replaced as a whole.
- **Replace when the successor is accepted.** The old ADR is set to `superseded` in the same change that sets its successor to `accepted`, and its `superseded_on` equals the successor's `accepted_on`. A `proposed` successor replaces nothing: it may list the planned IDs in `supersedes`, but the old ADR's status stays until the successor is accepted.
- **Banner.** A `superseded` or `deprecated` ADR has a one-line note directly after its H1. The superseded banner is dated with `superseded_on`, and the deprecated banner with `deprecated_on`:

  ```markdown
  > **置き換え済み（YYYY-MM-DD）**: このADRの決定は現在有効ではない。現行の決定は[ADR-XXXX](XXXX-....md)を読む。
  ```

  ```markdown
  > **廃止（YYYY-MM-DD）**: このADRの決定は現在有効ではない。理由: ...
  ```

- **Append-only.** An ADR is append-only. Later, only `status`, `accepted_on`, `superseded_by`, `superseded_on`, `deprecated_on`, `amended_by`, and the banner line may change, and these changes need no new ADR. `supersedes` and `amends` are not among them: they are written together with the body (the reason for the replacement and the carried-over decisions) when the replacing or amending ADR is written. Any other change to the body (adding, changing, or removing a decision) is made by a new ADR that replaces the old one as a whole. `updated` stays the last content change and does not move when only these fields change.
- **Index.** A change that alters an ADR's status updates the tables in [adr/README.md](adr/README.md) in the same change. New-form rows follow the four-digit rows in `accepted_on` order.

## Validation

Future documentation validation should check unique IDs, allowed status values, date formats, links in `related`, `depends_on`, `superseded_by`, and `supersedes`, the ADR status and field combinations and matching dates, and the filename convention. `scripts/check-adr-numbers.sh` already checks the ADR IDs: unique four-digit numbers and new-form IDs, the frontmatter `id` matching the filename, the branch number being present, and a new-form filename's date matching `accepted_on`:

```text
docs/adr/0001-rust-runtime.md
docs/adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md
docs/design/supervisor-lifecycle.md
docs/plans/rust-runtime-mvp.md
```
