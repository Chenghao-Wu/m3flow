# Writing tasks and providers

A task = a YAML contract (`task/v1`) + at least one provider implementation.

## 1. The contract

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
  result: {type: ViscosityResult}   # add the type to crates/core/src/atypes.rs if new
requirements:
  needs_time_axis: true
  dynamics_sensitive: true
  resolution: [atomistic]
validation: []                       # validators the provider must report
implementations:
  - {provider: analysis, default: true}
resources: {cpu: 2, walltime: 30 min}
```

Rules of thumb:

- Outputs always come back as files + a `data` payload for anything other
  steps/conditions might reference (`${step.result.data.value}`).
- Validators are the scientific gate: declare what the provider can honestly
  check; the runtime fails the task when a declared validator is missing or
  failing.
- If a task's logic changes meaningfully, bump `version`. Cache keys include
  `name@version` and the provider version — correctness depends on it.

## 2. The provider

Providers are executables `m3flow-<name>` on PATH (or configured in
`m3flow.yaml`) speaking `m3flow-provider/1` (see
`docs/provider-protocol.md`). In Python, the shared runtime does the plumbing
(`providers/src/m3flow_provider/__init__.py`):

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

- **stdout carries exactly one JSON document.** Logs go to stderr/files.
- Outputs are *staged*: files written under `workdir`, referenced relative.
  The runtime ingests them into the CAS — never move/rename them afterwards.
- Errors: raise `ProviderFailure(error_type, category, message,
  recoverable=..., details=..., raw_log=...)`; unexpected exceptions become
  `engine_crash/provider_error`. Categories drive the scheduler's retry.
- `describe` must report the engine version accurately — it joins the cache
  key. Bump the provider's own version when task logic changes.
- Input artifacts' `files` are absolute CAS paths (extensionless). Stage
  copies with proper filenames if your parser sniffs extensions (see
  `_universe` in the analysis provider).

## 3. Register and test

Drop the YAML in a project's `tasks/` (shadows builtins) or the library
`tasks/`, then:

```bash
m3flow task inspect compute_viscosity        # loaded + schema-valid
m3flow-analysis describe                     # task visible in provider
m3flow workflow validate workflows/x.yaml    # workflows using it type-check
```
