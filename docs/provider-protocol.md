# M3Flow Provider Protocol (m3flow-provider/1)

A **Provider** adapts an external scientific tool (AutoPoly, LAMMPS, Python analysis, …) to M3Flow tasks.
Providers are independent OS processes; the runtime communicates with them exclusively through
JSON documents on stdout/stdin and files in a task working directory. Languages are fully decoupled.

## Executable convention

Each provider ships one executable named `m3flow-<provider>` on `PATH` (or configured in `m3flow.yaml`):

```bash
m3flow-lammps describe  [--json]
m3flow-lammps validate  REQUEST.json
m3flow-lammps execute   REQUEST.json
m3flow-lammps diagnose  REQUEST.json
```

All output is a single JSON document on **stdout**. Human chatter goes to stderr and is ignored by the runtime.
Exit code: `0` for a well-formed response (including scientific failures), non-zero only for protocol violations.

## describe

```json
{
  "protocol": "m3flow-provider/1",
  "provider": {"name": "lammps", "version": "1.0.0"},
  "engine": {"name": "LAMMPS", "version": "stable_22Jul2025_update3", "path": "/home/zhenghaowu/lammps/build/lmp"},
  "tasks": [{"name": "run_npt", "version": "1.0.0"}, {"name": "energy_minimize", "version": "1.0.0"}],
  "validators": ["simulation_completed", "no_nan", "no_lost_atoms", "trajectory_readable"]
}
```

## validate

Checks an execute request without running it (input files exist, parameters coherent, engine reachable).

```json
{"valid": true, "errors": []}
```

## execute

### Request

```json
{
  "protocol": "m3flow-provider/1",
  "task": {"name": "run_npt", "version": "1.0.0"},
  "workflow_run_id": "wr_4f2a9c10",
  "task_run_id": "tr_17b2aa01",
  "workdir": "/abs/path/.m3flow/runs/wr_4f2a9c10/tr_17b2aa01",
  "inputs": {
    "state": {
      "id": "art_91ab22cd",
      "type": "SimulationState",
      "schema_version": "1",
      "files": {"data": "/abs/.../system.data", "restart": "/abs/.../state.restart"},
      "metadata": {"units": "real", "atom_style": "full", "ensemble": "NVT", "n_atoms": 1210}
    }
  },
  "parameters": {"temperature": {"value": 300.0, "unit": "K"}, "duration": {"value": 50.0, "unit": "ps"}},
  "resources": {"cpu": 8},
  "config": {"engine": {"path": "/home/zhenghaowu/lammps/build/lmp", "mpi": false, "np": 8}}
}
```

- `inputs.<name>.files` are **absolute** paths into the artifact store. Providers must never write into them.
- All files a provider wants returned as outputs must be written inside `workdir`.
- `parameters` are canonicalized by the core (quantities become `{"value","unit"}` in canonical units).
- `execute` may run on a **different host** than the m3flow driver (e.g. a
  compute node under the [Slurm executor](slurm.md)): rely only on `workdir`
  and the declared input paths, never on the driver's environment.

### Response (success)

```json
{
  "status": "success",
  "outputs": {
    "state": {
      "type": "SimulationState",
      "files": {"data": "state.data", "restart": "state.restart"},
      "metadata": {"ensemble": "NPT", "temperature": {"value": 300.0, "unit": "K"}, "n_atoms": 1210}
    },
    "trajectory": {
      "type": "ProductionTrajectory",
      "files": {"trajectory": "traj.lammpstrj"},
      "metadata": {"ensemble": "NPT", "observables": ["coordinates", "box", "energy", "pressure"]}
    }
  },
  "validation": [{"name": "no_nan", "passed": true, "detail": null}],
  "engine": {"name": "LAMMPS", "version": "stable_22Jul2025_update3"},
  "warnings": []
}
```

- Output `files` are paths **relative to `workdir`**. The runtime ingests them into the content-addressed store.
- Result-type outputs (e.g. `DensityResult`) additionally carry an inline `data` object with the numeric payload.
- Every validator named by the TaskSpec must appear in `validation` with a boolean verdict.

### Response (scientific failure)

```json
{
  "status": "error",
  "error": {
    "error_type": "lost_atoms",
    "category": "simulation_instability",
    "recoverable": true,
    "provider": "lammps",
    "task": "run_npt",
    "message": "Lost 8 atoms of 5000",
    "details": {"original_atoms": 5000, "remaining_atoms": 4992},
    "raw_log": "log.lammps"
  },
  "partial_outputs": {}
}
```

Standard `error_type`s: `lost_atoms`, `nan_detected`, `energy_blowup`, `trajectory_corrupt`,
`simulation_incomplete`, `engine_crash`, `engine_missing`, `input_invalid`, `builder_failed`,
`type_check_failed`, `validation_failed`. Standard `category`s: `simulation_instability`,
`input_error`, `environment_error`, `resource_error`, `protocol_error`, `scientific_validation`.

## diagnose

Best-effort post-mortem on a failed request (reads `workdir`, logs). Returns
`{"diagnostics": {...}, "suggestions": [...]}`. M3Flow reports facts; repair strategy belongs to the caller.

## Versioning

The protocol version string (`m3flow-provider/1`) is sent in every request and returned by `describe`.
A runtime refuses providers whose major protocol differs.
