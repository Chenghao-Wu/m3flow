# M3Flow — The Complete Manual

A **task-centric, artifact-driven, provenance-first** workflow runtime for
molecular simulation. M3Flow sits between coding agents (Claude Code, etc.)
or human researchers and simulation software (AutoPoly, LAMMPS), providing a
stable, discoverable, composable, and fully tracked execution environment.

```
SystemSpec ──▶ tasks ──▶ artifacts ──▶ workflows ──▶ results
                  │            │
                  └── provenance + content-addressed cache ──┘
```

This manual consolidates everything: concepts, the CLI, the JSON contract,
the provider protocol, the task and workflow libraries, project
configuration, the TUI, and the reference runs. Shorter focused documents
live alongside it in `docs/`; this file is the single-page reference.

---

## Table of contents

1. [Concepts](#1-concepts)
2. [Installation](#2-installation)
3. [Quickstart](#3-quickstart)
4. [The artifact type system](#4-the-artifact-type-system)
5. [Projects and configuration](#5-projects-and-configuration)
6. [The runtime](#6-the-runtime)
7. [Provenance](#7-provenance)
8. [The CLI](#8-the-cli)
9. [The agent-facing JSON contract](#9-the-agent-facing-json-contract)
10. [The provider protocol (m3flow-provider/1)](#10-the-provider-protocol)
11. [The task library](#11-the-task-library)
12. [The workflow library](#12-the-workflow-library)
13. [Authoring tasks and providers](#13-authoring-tasks-and-providers)
14. [Authoring workflows](#14-authoring-workflows)
15. [The TUI](#15-the-tui)
16. [Reference runs](#16-reference-runs)
17. [Design invariants](#17-design-invariants)
18. [Repository layout](#18-repository-layout)

---

## 1. Concepts

Everything in M3Flow is one of four primitives: **task**, **artifact**,
**workflow**, **run**.

### 1.1 Tasks

A task is the atomic unit of work: versioned (`name@semver`), typed,
validated. Tasks are declared in YAML against the `task/v1` schema and
loaded from the embedded library (`tasks/`) or a project's own `tasks/`
directory — a project task shadows the builtin at the same `name@version`.

A task spec declares:

- **inputs / outputs** — named ports with an artifact `type`; inputs may be
  `required` and/or `many` (a fan-in list).
- **parameters** — typed: `string | integer | number | boolean | enum |
  string_list | number_list | map`, plus quantity types `temperature |
  pressure | time | length | density | energy | area`. Quantity values must
  carry units: `{value: 300, unit: K}` or the string form `"300 K"`. **Bare
  numbers are rejected.** Canonical units: K, bar, fs, Å, g/cm³, kcal/mol,
  Å².
- **requirements** — scientific preconditions (`ensemble`, `observables`,
  `needs_time_axis`, `dynamics_sensitive`, `resolution`).
- **validation** — validator names the provider MUST report in every
  response; a failing or missing validator fails the task with
  `scientific_validation`.
- **implementations** — which providers can execute the task;
  `default: true` picks the one used unless overridden.

Run tasks (`run_nvt`, `run_npt`, `run_nve`, `run_deform`, …) take exactly one
of `system: SimulationSystem` or `state: SimulationState` and produce a new
`state` — chains of MD stages are `state → state` edges.

### 1.2 Artifacts

Artifacts are immutable, typed, content-addressed records:

```
{id: art_…, type, schema_version, files, metadata, data, content_hash,
 producer, created_at}
```

- Files live in the content-addressed store (CAS) at
  `.m3flow/artifacts/sha256/<2>/<hash>`; identical bytes are stored once.
- `content_hash` = hash(type + schema_version + per-file sha256). Metadata
  is deliberately excluded from identity — it describes, it never
  identifies.
- `data` is a small inline JSON payload (e.g. a DensityResult's
  `value/unit`) that workflow conditions and `${step.out.data.field}`
  references read.
- The type hierarchy is nominal subtyping (§4): a `SimulationState` input
  accepts an `EquilibratedState`.

### 1.3 Workflows

A workflow (`workflow/v1` YAML) composes tasks — and other workflows — into
a DAG with a small expression language. The compiler expands everything
**statically**; there are no runtime loops.

- **Declaration order is semantic** — a step may only reference earlier
  steps.
- **stages** — MD shorthand: `{ensemble: npt, temperature: 300 K,
  duration: 50 ps}` expands to a chained run task. `minimize`'s `duration`
  maps to `relax_duration`.
- **foreach** — `foreach: ${params.temperatures} as: T` expands statically
  at compile time into `step__0`, `step__1`, … (max 256). Referencing the
  base name (`${sweep.thermo}`) in a `many: true` input collects all
  expansions in order.
- **conditions** — `"${check.report.equilibrated}"`, `"not … and …"` — a
  false condition marks the node SKIPPED, and dependents are SKIPPED
  transitively.
- **subworkflows** — `workflow: name` inlines the child at compile time
  with `step.` id prefixing; child inputs bind to parent expressions, child
  outputs become `${step.output}` in the parent. Nesting depth ≤ 8.
- Protocol workflows carry scientific metadata (`domain`, `purpose`,
  `references`, `assumptions`) and are **immutable once published** — fork
  a new `name@version` to change a schedule.

### 1.4 The equilibration gate

An **EquilibratedState** can only come into existence through a passing
**EquilibrationReport** — via the `promote_equilibrated_state` task.
Finishing a protocol (the last MD stage completing) is never treated as
proof of equilibration. This is enforced by the type system plus the
runtime, not by convention.

---

## 2. Installation

```bash
# Rust core + CLI + TUI
cargo build --release          # binary: target/release/m3flow

# Providers (Python; JSON-protocol processes on PATH)
pip install -e providers/      # m3flow-autopoly, m3flow-lammps, m3flow-analysis

# Engines
#   AutoPoly: importable in the provider's Python environment
#   LAMMPS:   `lmp` on PATH, or configure in m3flow.yaml:
#             providers: {lammps: {engine: {executable: /path/to/lmp}}}
```

Verify:

```bash
m3flow provider list       # all providers "ok", engine versions shown
m3flow task list           # the 19-task atomic library
m3flow workflow list       # the 18-workflow library
```

---

## 3. Quickstart

```bash
m3flow init --name demo
cd demo

cat > systems/ethanol.yaml <<'EOF'
schema: system/v1
name: ethanol
components:
  - id: ethanol
    type: molecule
    representation: {type: smiles, value: "CCO"}
    count: 50
environment:
  type: bulk
  target_density: {value: 0.789, unit: g/cm3}
resolution: {type: atomistic, force_field: oplsaa}
EOF

m3flow workflow run construct_system --input specification=@systems/ethanol.yaml
m3flow workflow run simple_equilibration --input system=art_<from previous>
m3flow run list && m3flow run inspect <wr_…>
m3flow tui                    # live cockpit
```

Discovery is part of the contract — everything an agent needs is one CLI
call away:

```bash
m3flow task search polymer                        # full-text task search
m3flow task inspect run_npt                       # full typed contract
m3flow workflow plan polymer_21step_equilibration # static expansion
m3flow schema show system                         # SystemSpec JSON schema
m3flow artifact lineage art_…                     # full upstream tree
```

Every command accepts `--json`; see §9.

---

## 4. The artifact type system

Nominal subtyping over five families (`Spec`, `System`, `State`, `Dataset`,
`Result`) plus the open root `Artifact`. Compatibility: `have <: want` — a
subtype is accepted wherever a supertype is declared
(`m3flow artifact compatible art_… <task>` answers this directly).

```
Artifact
├── Spec
│   └── SystemSpec
├── System
│   ├── MolecularSystem
│   ├── ParameterizedSystem
│   └── SimulationSystem
├── State
│   └── SimulationState
│       └── EquilibratedState
├── Dataset
│   ├── Trajectory
│   │   └── ProductionTrajectory
│   ├── SimulationLog
│   ├── ThermodynamicSeries
│   │   └── TemperatureSeries
│   └── StressStrainSeries
└── Result
    ├── DensityResult      ├── RDFResult        ├── RgResult
    ├── ReeResult          ├── MSDResult        ├── DiffusionResult
    ├── CTEResult          ├── TgResult         ├── AdhesionResult
    ├── ModulusResult      └── EquilibrationReport
```

`m3flow schema list` prints this from the binary itself; the hierarchy is
defined in `crates/core/src/atypes.rs`.

---

## 5. Projects and configuration

`m3flow init` creates a project directory rooted at `m3flow.yaml`:

```yaml
schema: m3flow-project/v1
name: my_project
registries:                    # extra directories to load specs from
  tasks: [./tasks]
  workflows: [./workflows]
providers:
  lammps:
    executable: /path/to/m3flow-lammps   # default: m3flow-<name> on PATH
    python: /path/to/python              # for Python providers
    engine: {executable: /path/to/lmp, mpi: false, np: 8}
    extra: {...}                          # free-form, forwarded in `config`
defaults: {...}
```

Project layout after a run:

```
m3flow.yaml
systems/           workflows/        tasks/         # user-authored specs
.m3flow/
  artifacts/sha256/<2>/<hash>   # content-addressed store
  runs/<wr_…>/<tr_…>/           # per-task workdirs, request/response, logs
  m3flow.db                     # SQLite provenance store
```

The CLI discovers the project by walking upward from the cwd; outside a
project, registry commands still work with builtins.

---

## 6. The runtime

`m3flow workflow run` performs: **compile → bind → execute**.

1. **Compile** — static expansion of stages/foreach/subworkflows into a
   DAG; every reference resolved; full type check against the artifact
   hierarchy. `m3flow workflow plan <name>` shows exactly this compiled
   form before anything executes.
2. **Bind** — inputs come from `--input name=art_…` (existing artifact) or
   `name=@file`. SystemSpec files are schema-validated and registered as
   `Spec` artifacts, so provenance chains start at the human-authored spec.
3. **Execute** — a concurrent local scheduler runs ready nodes
   (`--max-concurrency` to cap), dispatching each node to its provider
   process over `m3flow-provider/1` and ingesting returned files into the
   CAS.

### 6.1 Caching

Before executing, a node looks up:

```
hash(task@version, provider@version, engine version,
     input content hashes, canonical params)
```

A hit marks the node **CACHED** and links the original artifacts —
provenance stays complete, and a repeated workflow is nearly instant (the
reference `polymer_multi` run is 35/35 CACHED in ~4 s). `--no-cache`
bypasses; `m3flow cache clear` drops entries (artifacts are kept).

Correctness depends on versioning discipline: if a task's logic changes
meaningfully, bump its `version`; the provider version and engine version
join the key automatically via `describe`.

### 6.2 Failure taxonomy, retry, resume

Every failure carries a structured `error_type` + `category`
(`input_error`, `provider_error`, `execution_error`,
`scientific_validation`, `environment_error`) — agents branch on these
programmatically. Per-step `retry: {max_attempts, on: [categories]}`
retries recoverable categories automatically.

- `m3flow run resume <wr>` — keep completed steps, re-run the rest.
- `m3flow run retry <wr> <step>` — re-execute one step and everything
  downstream of it.
- `m3flow run cancel <wr>` — request cancellation.

Provider task failures include `raw_log` (the engine log tail) and
`m3flow run logs <wr> [--step …]` surfaces the full request/response and
workdir files.

---

## 7. Provenance

`.m3flow/m3flow.db` records every workflow_run, task_run (status, attempts,
timestamps, params, validation verdicts, engine), artifact (+files), and
every input/output edge.

```bash
m3flow artifact lineage art_c8399702   # recursive tree back to the SystemSpec
m3flow artifact inspect art_…          # files, metadata, data payload, producer
m3flow run graph <wr_…>                # nodes, edges, statuses
```

Because artifacts are content-addressed and cache hits *link* rather than
copy, the lineage of any result is always complete and exact — you can
answer "which equilibration protocol, at what temperature, with which
engine build produced this density?" for any artifact in the store.

---

## 8. The CLI

Global: `--json` on every command (stdout = one JSON document; progress to
stderr). Exit codes for runs: `0` COMPLETED, `2` FAILED, `3` CANCELLED;
`1` for command errors.

| command | what it does |
|---|---|
| `init [dir] [--name N]` | initialize a project |
| `task list [--category C]` | list registered tasks |
| `task search <query>` | full-text search over names/descriptions/tags |
| `task inspect <name>` | full task spec as JSON |
| `workflow list` | list registered workflows |
| `workflow inspect <name>` | full workflow spec as JSON |
| `workflow validate <name_or_file>` | schema-validate a workflow |
| `workflow plan <name> [--param k=v]` | compiled DAG, no execution |
| `workflow run <name> [--input n=art/@f] [--param k=v] [--no-cache] [--max-concurrency N] [--dry-run]` | execute |
| `run list [--limit N]` | recent runs |
| `run inspect <wr>` | run record + per-step status |
| `run logs <wr> [--step prefix]` | per-step response documents, workdir files |
| `run graph <wr>` | dependency graph with statuses |
| `run resume <wr>` | resume, keeping completed steps |
| `run retry <wr> <step>` | re-execute one step + downstream |
| `run cancel <wr>` | request cancellation |
| `artifact list [--type T] [--limit N]` | newest artifacts |
| `artifact inspect <art>` | files/metadata/data/producer |
| `artifact lineage <art>` | recursive provenance tree |
| `artifact compatible <art> <task>` | can this artifact feed that task? |
| `artifact register --type T --file name=path [--meta JSON] [--data JSON]` | register existing files |
| `schema list` | artifact type hierarchy + document schemas |
| `schema show <task\|workflow\|system\|artifact>` | print a JSON schema |
| `provider list` | providers, availability, engine versions |
| `provider diagnose <name>` | locate + `describe` a provider |
| `cache stats` / `cache clear` | cache inspection and management |
| `tui [wr]` | interactive execution cockpit |

IDs: `art_…` artifact, `tr_…` task run, `wr_…` workflow run. Commands take
full ids (8 hex chars); `run logs --step` accepts a unique step prefix.

---

## 9. The agent-facing JSON contract

M3Flow is designed to be driven programmatically. With `--json`, stdout
carries exactly one JSON document; diagnostics go to stderr.

**Success documents** (stable keys):

- `workflow run --json` → the WorkflowRun record `{id, name, version,
  status, inputs, params, outputs, error, created_at, started_at,
  ended_at, spec_hash, workdir, git}`. `outputs` maps names to artifact
  ids; outputs of SKIPPED branches are absent.
- `run inspect --json` → `{run: {...}, tasks: [TaskRunRecord…]}` with
  `{id, node_id, task_name, task_version, provider, status, attempts,
  params, error, validation, engine, started_at, ended_at, cache_key}`.
- `run graph --json` → `{nodes: [{id, task, label, status}], edges:
  [{from, to}]}`.
- `artifact inspect --json` → `{id, type, schema_version, content_hash,
  files, metadata, data, producer}`.
- `artifact lineage --json` → recursive `{id, type, producer: {task_run…,
  inputs: [...]}}`.
- `artifact compatible --json` → `{compatible, matching_inputs: [{input,
  declared_type}]}`.

**Errors** — a single document:

```json
{
  "error_type": "schema | type | workflow | task | provider | execution |
                 scientific_validation | artifact_compatibility | not_found |
                 io | internal",
  "category": "input_error | protocol_error | task_error | provider_error |
               execution_error | scientific_validation | compatibility_error |
               not_found | environment_error | internal",
  "recoverable": false,
  "message": "human-readable",
  "...": "variant-specific fields"
}
```

Branch on `category` (retry policy) and `error_type` (semantics). Provider
failures add `raw_log` with the engine log tail.

---

## 10. The provider protocol

Providers adapt external tools (AutoPoly, LAMMPS, Python analysis) to
M3Flow tasks. They are independent OS processes named `m3flow-<provider>`,
communicating exclusively through JSON on stdout/stdin — languages fully
decoupled. Protocol: **`m3flow-provider/1`**.

```bash
m3flow-lammps describe  [--json]
m3flow-lammps validate  REQUEST.json
m3flow-lammps execute   REQUEST.json
m3flow-lammps diagnose  REQUEST.json
```

Rules: exactly one JSON document on **stdout** (human chatter to stderr);
exit `0` for any well-formed response *including scientific failures*;
non-zero only for protocol violations.

- **describe** → `{protocol, provider: {name, version}, engine: {name,
  version, path}, tasks: [...], validators: [...]}`. The engine version
  joins the cache key — report it accurately.
- **validate** → `{valid, errors[]}` — check an execute request without
  running it.
- **execute** — request carries `{task, workflow_run_id, task_run_id,
  workdir, inputs, parameters, resources, config}`. Input files are
  **absolute CAS paths, never written to**; outputs must be staged inside
  `workdir` and returned as workdir-relative paths; quantities arrive
  canonicalized (`{value, unit}`). Success response: `{status: "success",
  outputs, validation: [{name, passed, detail}], engine, warnings}`. Every
  validator named by the TaskSpec must appear with a verdict. Scientific
  failure: `{status: "error", error: {error_type, category, recoverable,
  message, details, raw_log}, partial_outputs}`.
- **diagnose** — best-effort post-mortem: `{diagnostics, suggestions}`.
  M3Flow reports facts; repair strategy belongs to the caller.

Standard `error_type`s: `lost_atoms`, `nan_detected`, `energy_blowup`,
`trajectory_corrupt`, `simulation_incomplete`, `engine_crash`,
`engine_missing`, `input_invalid`, `builder_failed`, `type_check_failed`,
`validation_failed`. Standard categories: `simulation_instability`,
`input_error`, `environment_error`, `resource_error`, `protocol_error`,
`scientific_validation`.

The protocol version is sent in every request and returned by `describe`;
the runtime refuses providers whose major protocol differs.

---

## 11. The task library

19 atomic tasks in three groups. Full typed contract of each:
`m3flow task inspect <name>`.

### construction (AutoPoly)

| task | summary |
|---|---|
| `build_system` | force-field-agnostic geometry from a SystemSpec |
| `parameterize_system` | assign force-field atom types and charges |
| `prepare_simulation_system` | pack into a simulation box → runnable SimulationSystem |

### simulation (LAMMPS)

| task | summary |
|---|---|
| `energy_minimize` | potential-energy minimization of a system or state |
| `run_nvt` | canonical (NVT) MD |
| `run_npt` | isothermal-isobaric (NPT) MD |
| `run_nve` | microcanonical (NVE) MD — typical for dynamics production |
| `run_soft_pushoff` | soft-potential push-off for CG/overlapping systems |
| `run_deform` | uniaxial tensile deformation (fix deform, NVT) |

### analysis, sampling, validation

| task | summary |
|---|---|
| `compute_density` | mean density + standard error over the equilibrated NPT tail |
| `compute_msd` | mean-squared displacement vs lag time (unwrapped coords) |
| `compute_rdf` | g(r), optionally between atom-type selections |
| `compute_rg` | radius of gyration of polymer chains (by molecule id) |
| `compute_ree` | mean end-to-end distance of chains |
| `compute_adhesion` | work of adhesion from interaction energy |
| `collect_thermo_series` | fan-in a temperature sweep into one TemperatureSeries |
| `fit_diffusion` | Einstein-relation linear fit of MSD → D |
| `fit_cte` | volumetric thermal expansion coefficient α |
| `fit_tg` | bilinear V(T) fit → glass transition temperature |
| `fit_modulus` | Young's modulus from the linear stress-strain region |
| `check_polymer_equilibration` | density drift, energy drift, … → EquilibrationReport |
| `promote_equilibrated_state` | SimulationState → EquilibratedState, gated on a passing report |

---

## 12. The workflow library

18 workflows in three layers. Inspect any of them with
`m3flow workflow inspect` / `workflow plan`.

### preparation

| workflow | summary |
|---|---|
| `construct_system` | SystemSpec → runnable SimulationSystem (all three construction stages) |
| `simple_equilibration` | minimize → NVT → NPT at the target state point |
| `polymer_21step_equilibration` | Larsen 21-step compression/decompression for polymer melts |
| `equilibrate_polymer` | adaptive: run the base protocol, check, extend only if the check fails |
| `cg_push_off` | CG melt preparation: minimize, soft push-off, NVT |
| `interface_relaxation` | polymer/substrate interface: gentle minimize + low-NVT relaxation |

### production

| workflow | summary |
|---|---|
| `npt_thermodynamic_production` | NPT production from an equilibrated state → ThermodynamicSeries |
| `dynamics_production` | NVE production → unwrapped ProductionTrajectory |
| `temperature_sweep` | NPT at each of a list of temperatures → TemperatureSeries |
| `mechanical_deformation` | tensile deformation → StressStrainSeries |

### properties

| workflow | summary |
|---|---|
| `density` | mean density from an NPT series (equilibrated tail) |
| `diffusion` | MSD from an unwrapped trajectory + Einstein fit |
| `rdf` | radial distribution function |
| `cte` | volumetric thermal expansion coefficient |
| `tg` | glass transition temperature from bilinear V(T) |
| `adhesion` | work of adhesion from an interface series |
| `mechanical_properties` | Young's modulus from a stress-strain series |
| `polymer_basic_properties` | one-shot panel for an equilibrated polymer |

Protocol workflows are versioned and immutable — fork `name@new-version`,
never edit a published schedule in place.

---

## 13. Authoring tasks and providers

A task = a YAML contract (`task/v1`) + at least one provider implementation.

```yaml
schema: task/v1
name: compute_viscosity
version: 1.0.0
description: Green-Kubo viscosity from an NVE trajectory.
category: analysis            # construction|simulation|sampling|analysis|validation|utility
inputs:
  trajectory: {type: Trajectory, required: true}
parameters:
  temperature: {type: temperature, required: true}
  block_length: {type: time, default: 10 ps}
outputs:
  result: {type: ViscosityResult}   # new types go in crates/core/src/atypes.rs
requirements:
  needs_time_axis: true
  dynamics_sensitive: true
  resolution: [atomistic]
validation: []
implementations:
  - {provider: analysis, default: true}
resources: {cpu: 2, walltime: 30 min}
```

Rules of thumb:

- Outputs come back as files + a `data` payload for anything other
  steps/conditions reference (`${step.result.data.value}`).
- Validators are the scientific gate — declare only what the provider can
  honestly check.
- Bump `version` when logic changes meaningfully; cache keys include
  `name@version` and the provider version.

In Python, the shared runtime (`providers/src/m3flow_provider/`) does the
plumbing:

```python
from m3flow_provider import Provider, ProviderFailure, artifact, verdict

def compute_viscosity(req):
    # req: protocol, task, workdir, inputs (ABSOLUTE store paths),
    #      parameters (canonical), resources, config
    traj = req["inputs"]["trajectory"]["files"]["dcd"]
    temp = req["parameters"]["temperature"]["value"]   # always Kelvin
    ...
    return {
        "outputs": {"result": artifact(
            "ViscosityResult",
            files={"json": "result.json"},       # workdir-relative
            data={"value": eta, "unit": "mPa*s"})},
        "validation": [verdict("trajectory_long_enough", ok, detail)],
    }

def cli():
    raise SystemExit(Provider(
        name="analysis", version="0.3.2",
        engine=lambda: {"name": "mdanalysis", "version": ...},
        tasks={"compute_viscosity": compute_viscosity},
    ).cli())
```

Contract points:

- stdout carries exactly one JSON document; logs go to stderr/files.
- Staged output files are ingested into the CAS by the runtime — never
  move/rename them after responding.
- Raise `ProviderFailure(error_type, category, message, recoverable=…,
  details=…, raw_log=…)`; unexpected exceptions become
  `engine_crash/provider_error`. Categories drive scheduler retry.
- Input artifact files are extensionless CAS paths — stage copies with
  proper filenames if your parser sniffs extensions.

Register by dropping the YAML into a project's `tasks/` (shadows builtins)
or the library `tasks/`, then:

```bash
m3flow task inspect compute_viscosity
m3flow-analysis describe
m3flow workflow validate workflows/x.yaml
```

---

## 14. Authoring workflows

```yaml
schema: workflow/v1
name: my_protocol
version: 1.0.0
description: What it does, when to use it.
kind: scientific_protocol
domain: [polymer, atomistic]
purpose: [equilibration]
references: ["citation strings"]
assumptions: ["explicit scientific assumptions"]

inputs:
  system: {type: SimulationSystem, required: true}
parameters:
  temperature: {type: temperature, default: 300 K}
  seed: {type: integer, default: 12345}

stages:                      # shorthand for linear MD protocols
  - {ensemble: minimize, name: min, duration: 5 ps, timestep: 0.5 fs}
  - {ensemble: nvt, name: nvt, temperature: "${params.temperature}", duration: 50 ps}

steps:
  check:
    task: check_polymer_equilibration
    inputs: {thermo: "${nvt.thermo}"}

outputs:
  state: {value: "${nvt.state}"}
  report: {value: "${check.report}"}
```

**References**: `${inputs.x}` (workflow input), `${step.output}` (artifact
of an earlier step), `${step.output.data.field}` / `.metadata.field`
(values inside the payload, usable in parameters and conditions),
`${params.x}`. Whole-string refs substitute the value; embedded refs
interpolate into strings (`"${T} K"` — building quantities from loop
variables).

**foreach**:

```yaml
steps:
  sweep:
    task: run_npt
    foreach: "${params.temperatures}"   # a JSON list
    as: T
    inputs: {state: "${inputs.state}"}
    parameters: {temperature: "${T} K"}
  collect:
    task: collect_thermo_series          # input declared many: true
    inputs: {series: "${sweep.thermo}"}  # gathers sweep__0..n in order
```

**conditions**: `condition: "not ${check.report.equilibrated}"` — boolean
mini-language (`and or not == != < <= > >=`, parens, literals, `${...}`
refs). False → SKIPPED, transitively. This is how `equilibrate_polymer`
extends NPT only when the first check fails.

**Discipline**:

- `m3flow workflow plan <name>` before every run — the compiled DAG is the
  contract; type errors and bad references surface there, not mid-run.
- Immutable once used: fork `name@new-version` to change a protocol.
- Keep quantities typed end-to-end (`"${params.temperature}"`, never
  pre-baked numbers).

---

## 15. The TUI

`m3flow tui [wr]` — the execution cockpit: run header, status-colored node
list, detail pane; auto-refreshes while the run is RUNNING.

| key | action |
|---|---|
| `j` / `k` | navigate nodes |
| `l` | logs pane |
| `a` | artifacts pane |
| `p` | provenance pane |
| `g` | graph pane |
| `r` | retry selected node |
| `R` | resume run |
| `q` | quit |

---

## 16. Reference runs

`examples/ref/` holds five reduced-scale end-to-end validations, all
reproducible via `./run_all.sh`:

| run | demonstrates | result |
|---|---|---|
| `ethanol_diffusion` | SMILES → equilibrate → NVE → MSD → D | full pipeline |
| `peo_density` | Larsen 21-step equilibration → density | 1.069 g/cm³ (lit. ~1.1) |
| `polymer_multi` | property fan-out + cache | 3rd run 35/35 CACHED in ~4 s |
| `peo_silica_adhesion` | quartz slab + film → work of adhesion | W = 101 mJ/m² |
| `cg_melt` | bead-spring CG construct → push-off → NVT | CG path |

Reduced scale demonstrates the platform, not converged physics: ps-scale
tails carry real statistical drift, and the equilibration gate says so —
that honesty is the feature.

---

## 17. Design invariants

Do not break these; correctness and trust depend on them.

- **Quantities carry units.** Bare numbers are rejected for quantity
  parameters — `{value, unit}` or `"300 K"` strings, canonicalized
  (K, bar, fs, Å, g/cm³, kcal/mol, Å²).
- **Artifact identity = type + schema version + per-file content hashes.**
  Metadata is descriptive, never identity.
- **Cache key = task@version + provider@version + engine version + input
  content hashes + canonical params.** Anything that can change a result
  must join the key — including passthrough config dicts.
- **EquilibratedState only via `promote_equilibrated_state`** gated on a
  passing EquilibrationReport.
- **Providers are separate processes** speaking `m3flow-provider/1` (one
  JSON document on stdout); the runtime owns ingestion into the CAS.
- **Protocols are immutable once published** — fork versions, never edit
  schedules in place.
- **Discovery is part of the contract** — every spec, schema, and status is
  reachable through the CLI with `--json`; agents never need to read
  source.

---

## 18. Repository layout

```
schemas/     JSON schemas (task/v1, workflow/v1, system/v1, artifact/v1)
tasks/       the 19-task atomic library (autopoly / lammps / analysis)
workflows/   the 18-workflow library (preparation / production / properties)
crates/      Rust workspace: core, registry, runtime, cli, tui
providers/   Python provider processes (m3flow-provider/1)
docs/        this manual + focused documents
examples/    reference projects (reduced-scale end-to-end runs)
```

Focused documents: `concepts.md`, `json-contract.md`,
`provider-protocol.md`, `writing-tasks.md`, `writing-workflows.md`,
`agent-benchmark.md` (a transcript of an agent completing "build a PEO
melt, equilibrate it, calculate density and diffusion at 300 K" using CLI
discovery only).
