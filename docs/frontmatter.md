---
id: docs-frontmatter
type: design
title: Documentation frontmatter specification
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
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
| `adr` | `proposed`, `accepted`, `rejected`, `superseded` | `superseded_by` when replaced |
| `design` | `draft`, `current`, `deprecated` | `last_verified`, optional `scope` |
| `plan` | `proposed`, `active`, `blocked`, `completed`, `archived` | optional `milestone`, `target`, `depends_on` |
| `journal` | `planned`, `open`, `done`, `abandoned` | optional `plan_step`, `queue_task`, `depends_on_journal`, `verify` |

An ADR is append-only. When a decision changes, create a new ADR and set the old one to `superseded` with `superseded_by`. Design documents describe the current state and may be edited. Plans describe intended work and may be edited while active. Journals record one task; their `Log` section is append-only, `queue_task` links to the cmux-taskq task ID after migration, and `verify` lists commands to pass to `cmux-taskq add --verify`. Journal IDs use `journal-NNN`.

## Validation

Future documentation validation should check unique IDs, allowed status values, date formats, links in `related`, `depends_on`, and `superseded_by`, and the filename convention:

```text
docs/adr/0001-rust-runtime.md
docs/design/supervisor-lifecycle.md
docs/plans/rust-runtime-mvp.md
docs/journal/003-supervisor.md
```
