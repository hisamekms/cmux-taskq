---
id: docs-frontmatter
type: design
title: Documentation frontmatter specification
status: current
created: 2026-09-21
updated: 2026-09-24
last_verified: 2026-09-24
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

`id`, `type`, `title`, `status`, `created`, and `updated` are required. `owners`, `tags`, and `related` are optional lists of strings. Dates use ISO 8601 calendar dates (`YYYY-MM-DD`). IDs are stable and use lowercase kebab-case, except ADR IDs which use `adr-NNNN`.

`updated` is the last content change. Design documents also use `last_verified` for the date on which the document was checked against the implementation.

## Type-specific fields

| Type | Allowed status | Additional fields |
| --- | --- | --- |
| `adr` | `proposed`, `accepted`, `rejected`, `superseded`, `deprecated` | `accepted_on`, `superseded_by`, `superseded_on`, `supersedes` (see [ADR fields](#adr-fields)) |
| `design` | `draft`, `current`, `deprecated`, `superseded` | `last_verified`, optional `scope` |
| `plan` | `proposed`, `active`, `blocked`, `completed`, `archived` | optional `milestone`, `target`, `depends_on` |
| `journal` | `draft`, `planned`, `open`, `done`, `abandoned` | optional `plan_step`, `queue_task`, `depends_on_journal`, `verify`. Frozen on 2026-09-22: no new journals are created, and existing files keep their values as written |

Design documents describe the current state and may be edited. Plans describe intended work and may be edited while active. Journals recorded one task each; their `Log` section was append-only, `queue_task` links to the dagq task ID after migration, and `verify` listed commands to pass to `dagq add --verify`. Journal IDs use `journal-NNN`. The `journal/` directory is frozen ([journal/README.md](journal/README.md)); the row above is kept so existing files still validate.

## ADR fields

The rules follow [ADR-0035](adr/0035-adr-is-superseded-whole-with-dates-and-banner.md). Only an `accepted` ADR is a current decision, and every decision in its body is in force.

| Status | Meaning |
| --- | --- |
| `proposed` | Under consideration. Its decisions are not in force yet |
| `accepted` | Adopted. Every decision in its body is in force |
| `rejected` | Not adopted |
| `superseded` | Replaced as a whole by the ADR in `superseded_by` |
| `deprecated` | Retired without a successor |

| Field | Carried by | Value |
| --- | --- | --- |
| `accepted_on` | `accepted`, `superseded`, `deprecated` | The date the ADR moved from `proposed` to `accepted`. A `rejected` ADR has none |
| `superseded_by` | `superseded` | One ADR ID of the successor. If the successor is itself superseded, the reader follows the chain to an `accepted` ADR |
| `superseded_on` | `superseded`, `deprecated` | The date the ADR became `superseded` or `deprecated` |
| `supersedes` | The replacing ADR | A list of the ADR IDs it replaces |

```yaml
status: superseded
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
superseded_by: adr-0040
superseded_on: 2026-09-25
```

- **Whole replacement.** An ADR that changes even one decision of an existing ADR rewrites and carries over the old ADR's decisions that are still in force, and the old ADR becomes `superseded` as a whole. One ADR may replace several (a consolidating ADR). Do not write a partial ADR that only says "this overrides decision N of ADR-XXXX".
- **Replace when the successor is accepted.** The old ADR is set to `superseded` in the same change that sets its successor to `accepted`, and its `superseded_on` equals the successor's `accepted_on`. A `proposed` successor replaces nothing: it may list the planned IDs in `supersedes`, but the old ADR's status stays until the successor is accepted.
- **Banner.** A `superseded` or `deprecated` ADR has a one-line note directly after its H1, dated with its `superseded_on`:

  ```markdown
  > **置き換え済み（YYYY-MM-DD）**: このADRの決定は現在有効ではない。現行の決定は[ADR-XXXX](XXXX-....md)を読む。
  ```

  ```markdown
  > **廃止（YYYY-MM-DD）**: このADRの決定は現在有効ではない。理由: ...
  ```

- **Append-only.** An ADR is append-only. Later, only `status`, `accepted_on`, `superseded_by`, `superseded_on`, and the banner line may change, and these changes need no new ADR. Any other change to the body (adding, changing, or removing a decision) is made by a new ADR that replaces the old one as a whole. `updated` stays the last content change and does not move when only these fields change.
- **Index.** A change that alters an ADR's status updates the tables in [adr/README.md](adr/README.md) in the same change.

## Validation

Future documentation validation should check unique IDs, allowed status values, date formats, links in `related`, `depends_on`, `superseded_by`, and `supersedes`, the ADR status and field combinations and matching dates, and the filename convention:

```text
docs/adr/0001-rust-runtime.md
docs/design/supervisor-lifecycle.md
docs/plans/rust-runtime-mvp.md
docs/journal/003-supervisor.md
```
