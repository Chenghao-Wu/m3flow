# Agent benchmark self-check (plan §66)

The benchmark question, given to an agent with only `m3flow --help`:

> Build a PEO melt, equilibrate it, calculate density and diffusion at 300 K.

This document is the transcript of that exercise performed against a fresh
project (reduced scale), using **CLI discovery only** — no reading source
files. Every command shown ran as-is.

## 1. Discover

```console
$ m3flow init --name agentbench
$ m3flow task search polymer          # check_polymer_equilibration, compute_rg, ...
$ m3flow workflow list                # construct_system, polymer_21step_equilibration, density, diffusion, ...
```

## 2. Learn the input shape

```console
$ m3flow schema show system           # SystemSpec JSON schema
$ m3flow task inspect build_system    # inputs/params/outputs/validators
```

From the schema's representation enum (`smiles|psmiles|sequence|name|file|
bead_spring`) the agent writes `systems/peo.yaml`:

```yaml
schema: system/v1
name: peo_bench
components:
  - id: peo
    type: polymer
    representation:
      type: psmiles
      first: "CCO[*]"
      middle: "[*]CCO[*]"
      last: "[*]CCO"
    degree_of_polymerization: 8
    number_of_chains: 6
environment:
  type: bulk
  target_density: {value: 1.1, unit: g/cm3}
resolution: {type: atomistic, force_field: oplsaa}
```

## 3. Compose and validate before executing

```console
$ m3flow workflow plan bench_run
workflow bench_run@1.0.0 — 32 steps
    1. construct.build              build_system@1.0.0
    ...
   27. equilibrate.promote          promote_equilibrated_state@1.0.0
   28. produce.npt                  run_npt@1.0.0  (after equilibrate.promote)
   ...
   32. diffusion.diffusion          fit_diffusion@1.0.0
```

`bench_run` composes five library workflows as subworkflows
(`construct_system` → `polymer_21step_reduced` →
`npt_thermodynamic_production` → `density` + `dynamics_production` →
`diffusion`). The plan command is where type errors and bad references
surface — not mid-run.

## 4. Execute

```console
$ m3flow workflow run bench_run --input specification=@systems/peo.yaml
  ...
run wr_76227507 finished: COMPLETED (workflow bench_run@1.0.0)
  output density: art_c8399702
  output diffusion: art_194acb78
  output report: art_8c6be54b
```

## 5. Inspect results and provenance

```console
$ m3flow artifact inspect art_c8399702
  DensityResult  {value: 1.067, std: 0.002, unit: g/cm3}      # lit. ~1.1
$ m3flow artifact inspect art_194acb78
  DiffusionResult {value: 4.8e-4, unit: cm2/s}                # ps-scale estimate
$ m3flow artifact lineage art_c8399702     # full tree back to the SystemSpec
```

## Checklist verdict (§66)

| capability | shown |
|---|---|
| discover tasks | `task search/list/inspect` |
| inspect schemas | `schema show system` |
| construct SystemSpec | `systems/peo.yaml` validated at ingestion |
| construct WorkflowSpec | `workflows/bench_run.yaml`, `workflow validate` |
| validate | `workflow plan` (compile + static type check) |
| execute | `workflow run` (32 nodes, concurrent scheduler) |
| inspect failure | structured errors + `run inspect/logs` (earlier runs exercised FAILED → repair → retry) |
| repair | `run retry <wr> <step>` re-executes a step + downstream |
| reuse equilibrium | `EquilibratedState` artifact reused across workflows via cache |
| reuse production | second identical run is 100% CACHED |
| extract results | `artifact inspect --json`, `.data` payloads |

Caveat honestly reported: reduced-scale numbers demonstrate the platform;
ps-scale tails are not converged physics, and the equilibration gate enforces
exactly that distinction.
