# M3Flow

A **task-centric, artifact-driven, provenance-first** workflow runtime for
molecular simulation. M3Flow sits between coding agents (Claude Code, etc.)
or human researchers and simulation software (AutoPoly, LAMMPS), providing a
stable, discoverable, composable, and fully tracked execution environment.

```
SystemSpec ──▶ tasks ──▶ artifacts ──▶ workflows ──▶ results
                  │            │
                  └── provenance + content-addressed cache ──┘
```

## Concepts in one paragraph

Everything is a **task** (a versioned, typed unit of work with declared
inputs/outputs/parameters and validators). Tasks exchange **artifacts**
(immutable, content-addressed, typed files + metadata + data payload).
Workflows compose tasks into DAGs with a small expression language
(`${step.output}`, conditions, `foreach`, subworkflows). The runtime compiles
a workflow statically, executes it with a concurrent scheduler, records every
input/output edge in a SQLite provenance store, and caches by content hash so
re-runs are instant. An **EquilibratedState** can only come into existence
through a passing `EquilibrationReport` — finishing a protocol is never
treated as proof of equilibration.

## Install

```bash
# Rust core + CLI + TUI
cargo build --release          # binary: target/release/m3flow

# Providers (Python, JSON protocol processes on PATH)
pip install -e providers/      # m3flow-autopoly, m3flow-lammps, m3flow-analysis

# Engines
#   AutoPoly: importable in the provider's Python env
#   LAMMPS:   `lmp` on PATH, or configure in m3flow.yaml:
#             providers: {lammps: {engine: {executable: /path/to/lmp}}}
```

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
m3flow run list && m3flow run inspect <wr_...>
m3flow tui                    # live cockpit
```

Every command accepts `--json`. Discovery is part of the contract:

```bash
m3flow task list              # all 19 atomic tasks
m3flow task inspect run_npt   # full typed contract of one task
m3flow workflow list          # 18 library workflows
m3flow workflow plan polymer_21step_equilibration   # static expansion
m3flow schema list            # artifact type hierarchy
m3flow artifact lineage art_… # full upstream provenance tree
```

## Layout

```
schemas/     JSON schemas (task/v1, workflow/v1, system/v1, artifact/v1)
tasks/       the 19-task atomic library (autopoly/lammps/analysis)
workflows/   the 18-workflow library (preparation/production/properties)
crates/      Rust workspace: core, registry, runtime, cli, tui
providers/   Python provider processes (m3flow-provider/1 protocol)
docs/        manual.md (complete reference) + focused docs
examples/    reference projects (reduced-scale end-to-end runs)
```

## The workflow library

| layer | workflows |
|---|---|
| preparation | `construct_system`, `polymer_21step_equilibration` (Larsen schedule), `simple_equilibration`, `equilibrate_polymer` (adaptive), `cg_push_off`, `interface_relaxation` |
| production | `npt_thermodynamic_production`, `dynamics_production`, `temperature_sweep`, `mechanical_deformation` |
| properties | `density`, `diffusion`, `rdf`, `cte`, `tg`, `adhesion`, `mechanical_properties`, `polymer_basic_properties` |

Protocols are **versioned and immutable**: change a schedule by forking a new
`name@version`, never by editing in place.

## Reference runs (examples/ref)

Reduced-scale end-to-end validations, all reproducible:

| run | result |
|---|---|
| `ethanol_diffusion` | SMILES → equilibrated → NVE → MSD → D |
| `peo_density` | Larsen 21-step → 1.069 g/cm³ (lit. ~1.1) |
| `polymer_multi` | property fan-out; 3rd run 35/35 CACHED in ~4 s |
| `peo_silica_adhesion` | quartz slab + film → W = 101 mJ/m² |
| `cg_melt` | bead-spring CG construct → push-off → NVT |

Reduced scale demonstrates the platform, not converged physics: ps-scale
tails carry real statistical drift, and the equilibration gate says so.

## Design invariants (do not break)

- Bare numbers are rejected for quantity parameters — `{value, unit}` or
  `"300 K"` strings, canonicalized (K, bar, fs, Å, g/cm³, kcal/mol, Å²).
- Artifact identity = type + schema version + per-file content hashes.
  Metadata is descriptive, never identity.
- Cache key = task@version + provider@version + engine version + input
  content hashes + canonical params.
- `EquilibratedState` only via `promote_equilibrated_state` gated on a
  passing `EquilibrationReport`.
- Providers are separate processes speaking `m3flow-provider/1` (single JSON
  document on stdout); the runtime owns ingestion into the CAS.

See `docs/manual.md` for the complete reference, `docs/` for focused documents.
