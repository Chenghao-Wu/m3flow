# M3Flow Documentation

A **task-centric, artifact-driven, provenance-first** workflow runtime for
molecular simulation.

## Start here

- **[The Complete Manual](manual.md)** — single-page reference covering
  concepts, the CLI, the JSON contract, the provider protocol, the task and
  workflow libraries, configuration, the TUI, and the reference runs.

## Focused documents

- [Concepts](concepts.md) — tasks, artifacts, workflows, runtime, provenance
- [Running on Slurm](slurm.md) — submit tasks as cluster jobs; config,
  resources mapping, failure taxonomy
- [Agent-facing JSON contract](json-contract.md) — `--json` output shapes and
  the error taxonomy
- [Provider protocol](provider-protocol.md) — `m3flow-provider/1`
  (describe / validate / execute / diagnose)
- [Writing tasks and providers](writing-tasks.md) — authoring guide
- [Writing workflows](writing-workflows.md) — authoring guide
- [Agent benchmark](agent-benchmark.md) — transcript of an agent building a
  PEO melt and computing density + diffusion using CLI discovery only

## Links

- [GitHub repository](https://github.com/Chenghao-Wu/m3flow)
