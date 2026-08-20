# M3Flow

**M**ultiscale **M**olecular **M**odeling work**FLOW** — a task-centric,
artifact-driven, provenance-first workflow runtime for molecular simulation.

M3Flow sits between coding agents (Claude Code, etc.) or human researchers and
computational software, providing a **stable, discoverable, composable, and
fully tracked** execution environment.

```
SystemSpec ──▶ tasks ──▶ artifacts ──▶ workflows ──▶ results
                  │            │
                  └── provenance + content-addressed cache ──┘
```

## Why M3Flow?

:fontawesome-solid-cubes: **Everything is a task**
:   A versioned, typed unit of work with declared inputs, outputs, parameters,
    and validators. 19 atomic tasks ship in the library.

:fontawesome-solid-box-archive: **Artifacts are immutable and content-addressed**
:   Typed files + metadata + data payload; identity is content, never names.
    Re-runs hit the cache and finish in seconds.

:fontawesome-solid-diagram-project: **Composable workflow DAGs**
:   A small expression language (`${step.output}`, conditions, `foreach`,
    subworkflows) compiles statically before anything runs.

:fontawesome-solid-clipboard-check: **Physics is gated, not assumed**
:   An `EquilibratedState` can only come into existence through a passing
    `EquilibrationReport` — finishing a protocol is never treated as proof
    of equilibration.

:fontawesome-solid-robot: **Agent-native by design**
:   Every CLI command accepts `--json`; discovery (`task list`,
    `task inspect`, `workflow plan`, `artifact lineage`) is part of the
    contract, so coding agents can drive simulations unsupervised.

:fontawesome-solid-server: **From laptop to cluster**
:   A built-in Slurm executor ships tasks to partitions with marker-primary
    tracking over a shared filesystem — no daemons, no agents on the cluster.

## Quickstart

```bash
m3flow init --name demo
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
m3flow run inspect <wr_...>     # or: m3flow tui — the live cockpit
```

## The workflow library

| layer | workflows |
|---|---|
| preparation | `construct_system`, `polymer_21step_equilibration` (Larsen schedule), `simple_equilibration`, `equilibrate_polymer` (adaptive), `cg_push_off`, `interface_relaxation` |
| production | `npt_thermodynamic_production`, `dynamics_production`, `temperature_sweep`, `mechanical_deformation` |
| properties | `density`, `diffusion`, `rdf`, `cte`, `tg`, `adhesion`, `mechanical_properties`, `polymer_basic_properties` |

Protocols are **versioned and immutable**: change a schedule by forking a new
`name@version`, never by editing in place.

## Where next?

- [Installation](install.md) — release binaries, providers, engines
- [Concepts](concepts.md) — the mental model in one sitting
- [The Complete Manual](manual.md) — the full single-page reference
- [Agent Benchmark](agent-benchmark.md) — an agent builds a PEO melt and
  computes density + diffusion using CLI discovery only
