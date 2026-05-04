# Functional Requirements (FR)

`docs/fr/` collects the **Functional Requirements documents** for roki.

## Position in the documentation stack

The **per-feature narrative** layer:

| Layer | Location | Axis | Primary readers |
|---|---|---|---|
| **Roadmap** | `.kiro/steering/roadmap.md` | Project-wide scope and the list of specs | Everyone / planning decisions |
| **FR (this directory)** | `docs/fr/<NN>-<feature>.md` | **Per-feature (Feature) narrative** (independent of any kiro spec) | Operators, new contributors, spec authors |
| **Reference** | [`docs/reference/`](../reference/) | Exhaustive lookup tables (CLI / config / artifacts / extension surface / log events) | Operators looking things up at runtime |
| **Requirements** | `.kiro/specs/<spec>/requirements.md` | Per EARS Acceptance Criterion | Implementers and verifiers |
| **Design** | `.kiro/specs/<spec>/design.md` | Architecture, data structures, interfaces | Implementers |

FR is designed so that **someone who wants to understand a feature can read just one file** and be done.
For that reason FR boundaries do not match kiro spec boundaries: features that span multiple specs (e.g. configuration, observability, agent tool boundary) are kept in a single file.

**FR and reference target different reader behaviors:**

- A reader of FR wants to understand "what is this feature, why does it exist, how does it behave" (read through the narrative).
- A reader of reference wants to immediately confirm "what does this flag / key / event mean" (look it up in a table).

Splitting them lets each side focus on its purpose.

## What to write / what not to write

Decision rule: **FR fixes the contracts where an operator or downstream spec author becomes a "reader" or "writer"**.
Internal type definitions, wire-format details, and library choices belong to design.

### Write

- **Purpose**: Why this feature is needed
- **User-visible Behavior**: How the feature appears from the operator / agent / downstream-spec perspective
- **Capabilities**: The main behaviors the feature provides (bullet list in prose)
- **Boundaries**: What it does NOT do, and the boundary against neighboring features
- **Operator-facing contract** ← FR fixes this:
  - **Keys and meaning of configuration files** (schema, namespace, default values, an outline of validation rules)
  - **CLI flags and their meaning** (flag name, what it overrides, how `--help` treats it)
  - **Path and required elements of public artifacts** (e.g. per-criterion fields of `review.md`, top-level fields of `distill-manifest.json`)
- **Traceability**: References to roadmap / each spec's requirements / design

### Do not write

- Internal type definitions, wire-level serialization details, chosen libraries (→ `design.md`)
- Individual Acceptance Criteria, concrete timeout seconds, fine-grained state-transition details (→ `requirements.md`)
- Implementation task breakdowns (→ `tasks.md`)

### Canonical references collected in one place

Cross-cutting contracts have a single **canonical reference table** under [`docs/reference/`](../reference/).
FR pages link to those tables and **do not restate** them.

| Reference | Canonical home |
|---|---|
| CLI flag list | [`docs/reference/cli.md`](../reference/cli.md) |
| Configuration schema (`roki.toml` / `WORKFLOW.md` all namespaces) | [`docs/reference/config.md`](../reference/config.md) |
| Public artifact paths / schemas | [`docs/reference/artifacts.md`](../reference/artifacts.md) |
| Extension surface (traits / hooks / context channels) | [`docs/reference/extension-surface.md`](../reference/extension-surface.md) |
| Structured log event list | [`docs/reference/log-events.md`](../reference/log-events.md) |

## Directory layout

```
docs/fr/
├── README.md             (this file)
├── INDEX.md              (feature × spec matrix)
├── _template.md          (skeleton for one file)
└── NN-<feature>.md ...   (one file per feature, flat layout)
```

For the feature list, see [INDEX.md](INDEX.md).

## Update flow

When adding a new feature:

1. Write `docs/fr/NN-<feature>.md` (FR comes first).
2. Write or update the `requirements.md` of the **kiro spec** that implements the feature.
3. Add a row to the matrix in `INDEX.md`.
4. If needed, reconcile with the Specs / Scope sections of `roadmap.md`.

When changing an existing feature:

- If you change `requirements.md` / `design.md`, update the **Capabilities** / **Boundaries** / **Traceability** of the corresponding FR page.
- Verify that the FR page does not contradict `roadmap.md`.

## Language

FR is written in **English**. requirements / design continue to follow the `language` setting in spec.json.

## Traceability conventions

- References to `requirements.md` use the form `<spec> Req N.M` (e.g. `roki-mvp Req 4.3`).
- References to a section in `design.md` quote the section title.
- References to `roadmap.md` use a path notation such as `Roadmap > Scope > In`.
