# Writing workflows

`workflow/v1` YAML. Compose tasks and other workflows; the compiler expands
everything statically — there are no runtime loops.

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

stages:                      # shorthand for MD protocols
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

## References

- `${inputs.x}` — a workflow input artifact.
- `${step.output}` — an output artifact of an earlier step.
- `${step.output.data.field}` / `.metadata.field` — a *value* inside the
  artifact payload (usable in parameters and conditions).
- `${params.x}` — a workflow parameter.
- Whole-string refs substitute the value; embedded refs interpolate into
  strings (`"${T} K"` — handy to build quantities from loop variables).

## stages

`{ensemble: minimize|nvt|npt|nve, temperature(_start/_end), pressure,
duration, timestep, name}` — expanded to chained run tasks (state→state) in
declaration order. The first stage binds `${inputs.system}`/`${inputs.state}`.
`minimize` duration maps to `relax_duration`. For anything beyond a linear
chain, use explicit `steps`.

## foreach

```yaml
steps:
  sweep:
    task: run_npt
    foreach: "${params.temperatures}"   # a JSON list
    as: T
    inputs: {state: "${inputs.state}"}
    parameters:
      temperature: "${T} K"
  collect:
    task: collect_thermo_series          # input declared many: true
    inputs: {series: "${sweep.thermo}"}  # gathers sweep__0..n in order
```

Expansion is static at compile time (max 256 items), so `m3flow workflow plan`
shows exactly what will run.

## conditions

`condition: "not ${check.report.equilibrated}"` — boolean mini-language
(`and or not == != < <= > >=` parens, literals, `${...}` refs). False → the
node is SKIPPED and its dependents too. Use for adaptive protocols
(`equilibrate_polymer` extends NPT only when the first check fails).

## subworkflows

`workflow: npt_thermodynamic_production` inlines the child with `step.`
prefixed node ids; the child's inputs bind to parent expressions, its outputs
become `${step.output}` in the parent. Nesting depth ≤ 8.

## outputs

`outputs: {name: {value: "${step.out}"}}`. Outputs referencing SKIPPED steps
are simply absent from `run.outputs` — document that for consumers.

## Discipline

- `m3flow workflow plan <name>` before every run — the compiled DAG is the
  contract.
- Immutable once used: fork `name@new-version` to change a protocol.
- Keep quantities typed end-to-end (`temperature: "${params.temperature}"`,
  never pre-baked numbers).
