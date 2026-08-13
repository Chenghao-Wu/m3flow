# M3Flow concepts

## Tasks

A task is the atomic unit: versioned (`name@semver`), typed, validated.
Declared in YAML (`task/v1` schema), loaded from `tasks/` (embedded library)
or a project's own `tasks/` directory (project shadows builtin on same
name@version).

- **inputs/outputs**: named, with an artifact `type`; `required`, `many`
  (fan-in list).
- **parameters**: typed — `string|integer|number|boolean|enum|string_list|
  number_list|map` plus quantity types `temperature|pressure|time|length|
  density|energy|area`. Quantity values must carry units:
  `{value: 300, unit: K}` or the string form `"300 K"`. Bare numbers are
  rejected. Canonical units: K, bar, fs, Å, g/cm³, kcal/mol, Å².
- **requirements**: scientific preconditions (ensemble, observables,
  needs_time_axis, dynamics_sensitive, resolution).
- **validation**: validator names the provider MUST report in every response;
  a failing validator fails the task with `scientific_validation`.
- **implementations**: which providers can execute it; `default: true` picks.

Run tasks (`run_nvt`, ...) take exactly one of `system: SimulationSystem` or
`state: SimulationState` — chained via `state` outputs.

## Artifacts

Immutable records: `{id: art_…, type, schema_version, files, metadata, data,
content_hash, producer, created_at}`.

- Files live in the CAS at `.m3flow/artifacts/sha256/<2>/<hash>`; identical
  bytes are stored once.
- `content_hash` = hash(type + schema_version + per-file sha256). Metadata is
  deliberately excluded from identity.
- `data` is a small JSON payload (e.g. a DensityResult's value/unit) that
  conditions and `${step.out.data.field}` references read.
- The type hierarchy is nominal subtyping: `EquilibratedState <:
  SimulationState <: State <: Artifact`, `TemperatureSeries <:
  ThermodynamicSeries <: Dataset`, `DensityResult <: Result`, ...
  (`m3flow schema list`). A `SimulationState` input accepts an
  `EquilibratedState`.

## Workflows

`workflow/v1` YAML: inputs, parameters, `steps` and/or `stages`, outputs.

- **Declaration order is semantic** — a step may only reference earlier
  steps.
- **stages** shorthand: `{ensemble: npt, temperature: 300 K, duration: 50 ps}`
  expands to a chained run task (state→state). `minimize`'s `duration` maps
  to `relax_duration`.
- **foreach**: `foreach: ${params.temperatures} as: T` expands statically at
  compile time (`step__0`, `step__1`, ...). Referencing the base name
  (`${sweep.thermo}`) in a `many: true` input collects all expansions in
  order.
- **conditions**: `"${check.report.equilibrated}"`, `"not ... and ..."` —
  false → node SKIPPED, dependents SKIPPED transitively.
- **subworkflows**: `workflow: name` inlines the child at compile time with
  `step.` id prefixing; child inputs bind to the parent's expressions.
- Protocol workflows carry scientific metadata (domain/purpose/references/
  assumptions) and are **immutable once published** — fork a new version to
  change a schedule.

## Runtime

- `m3flow workflow run` compiles (static expansion + type check), binds
  inputs (`--input name=art_…` or `name=@file`; SystemSpec files are
  schema-validated and registered as Spec artifacts), and executes with a
  concurrent local scheduler.
- **Cache**: before executing, a node looks up hash(task@version,
  provider@version, engine version, input content hashes, canonical params).
  A hit marks the node CACHED and links the original artifacts — provenance
  stays complete.
- **Retry**: per-step `retry: {max_attempts, on: [categories]}`; recoverable
  categories retry automatically.
- **Failure taxonomy**: structured `error_type` + `category` (input_error,
  provider_error, execution_error, scientific_validation, environment_error)
  everywhere — agents branch on them programmatically (all CLI has `--json`).
- **resume/retry**: `m3flow run resume <wr>` keeps completed steps;
  `run retry <wr> <step>` re-executes one step and its downstream.

## Provenance

`.m3flow/m3flow.db` records workflow_run, task_run (status, attempts,
timestamps, params, validation, engine), artifact(+files), and every
input/output edge. `m3flow artifact lineage art_…` walks the full tree;
`artifact compatible art_… <task>` answers "can this feed that task?".
