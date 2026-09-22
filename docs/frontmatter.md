---
id: docs-frontmatter
type: design
title: Documentation frontmatter specification
status: current
created: 2026-09-21
updated: 2026-09-23
last_verified: 2026-09-23
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
| `design` | `draft`, `current`, `deprecated`, `superseded` | `last_verified`, optional `scope` |
| `plan` | `proposed`, `active`, `blocked`, `completed`, `archived` | optional `milestone`, `target`, `depends_on` |
| `journal` | `draft`, `planned`, `open`, `done`, `abandoned` | optional `plan_step`, `queue_task`, `depends_on_journal`, `verify`. Frozen on 2026-09-22: no new journals are created, and existing files keep their values as written |

An ADR is append-only. When a decision changes, create a new ADR and set the old one to `superseded` with `superseded_by`. Design documents describe the current state and may be edited. Plans describe intended work and may be edited while active. Journals recorded one task each; their `Log` section was append-only, `queue_task` links to the dagq task ID after migration, and `verify` listed commands to pass to `dagq add --verify`. Journal IDs use `journal-NNN`. The `journal/` directory is frozen ([journal/README.md](journal/README.md)); the row above is kept so existing files still validate.

## Validation

Future documentation validation should check unique IDs, allowed status values, date formats, links in `related`, `depends_on`, and `superseded_by`, and the filename convention:

```text
docs/adr/0001-rust-runtime.md
docs/design/supervisor-lifecycle.md
docs/plans/rust-runtime-mvp.md
docs/journal/003-supervisor.md
```
