# Running on Slurm

With `executor.type: slurm`, m3flow submits each task to the cluster as a
Slurm job instead of running it locally. The provider protocol is unchanged:
the same `m3flow-<name> execute request.json` call runs inside a generated
batch script on a compute node, and results flow back through the shared
filesystem into the cache, provenance DB, and `results/` tree as usual.

## Requirements

- **Run m3flow on the cluster's login node.** The executor shells out to
  `sbatch` / `squeue` / `sacct` / `scancel`.
- **The project directory must live on a filesystem shared with the compute
  nodes** (GPFS/Lustre/NFS/BeeGFS — e.g. your cluster home, not `/tmp`).
  Workdirs, `request.json`, and output files are exchanged through it.
- Provider executables and engines must be usable from a compute node —
  typically via `setup_commands` (modules, conda) below, since home is
  shared.
- Works with or without Slurm accounting (`sacct`): completion is detected
  from an exit-code marker the batch script writes; accounting is only
  consulted to classify jobs killed by the scheduler (walltime, OOM, …).

## Configuration

```yaml
# m3flow.yaml
executor:
  type: slurm
  slurm:
    partition: gpua800          # optional; omit → cluster default
    account: my_allocation      # optional --account (required on many sites)
    qos: 4gpus                  # optional --qos
    gpu_type: a800              # resources.gpu: N → --gres=gpu:a800:N
    # gres: "gpu:a800:2"        # verbatim --gres; wins over gpu_type
    time: 24 h                  # default --time when a step declares no walltime
    poll_interval_secs: 15      # job-state poll cadence (±30% jitter)
    setup_commands:             # shell lines before the provider call
      - module load anaconda3
      - source activate autopoly
    extra_sbatch: []            # verbatim extra #SBATCH lines (site specifics)

providers:
  analysis:
    executor: local             # keep light providers on the login node
```

- **Executor precedence:** `--executor <local|slurm>` CLI flag >
  `providers.<name>.executor` > `executor.type` > `local`.
- **Resources → directives** (declared per task/step, see
  [writing-tasks](writing-tasks.md)):

  | `resources`        | `#SBATCH` directive                          |
  |--------------------|----------------------------------------------|
  | `cpu: 8`           | `--nodes=1 --ntasks=1 --cpus-per-task=8`     |
  | `gpu: 2`           | `--gres=gpu[:<gpu_type>]:2`                  |
  | `memory: 16GB`     | `--mem=16G`                                  |
  | `walltime: 30 min` | `--time=30` (also accepts `2 h`, `HH:MM:SS`) |

  MPI/threads *inside* the allocation stay the provider's business (e.g. the
  LAMMPS provider's `mpirun -np N` / OpenMP logic is unchanged; its
  `engine.launcher` may be set to `srun` on sites that prefer it).
- Executor settings are scheduling-only: they never join cache keys or
  artifact identity, so switching between `local` and `slurm` reuses the
  cache.

## Discovering your site's values

Partition/QoS/account names are site-specific. On XJTLU_XEC, `slurm-tool`
shows your QoS and storage; elsewhere:

```bash
sinfo                              # partitions, nodes, GRES
sacctmgr show assoc user=$USER     # accounts and QoS you may use
```

## What a run looks like

Per task, the workdir (`.m3flow/runs/<run>/<step>/`) gains:

- `submit.sh` — the generated batch script (inspect it when debugging),
- `slurm_job_id` — the submitted job id,
- `slurm-<jobid>.out` — Slurm's own output (setup command errors land here),
- `provider_stdout.json` / `provider_stderr.log` — the provider's protocol
  doc and log,
- `.m3flow_exit` — the provider exit code (primary completion signal),
- `request.json` / `response.json` — as with local execution.

`--max-concurrency` caps in-flight Slurm jobs (respect your QoS `MaxJobs`).
Queued time shows as `RUNNING` in the TUI/status.

## Failures

Slurm-level failures join the standard taxonomy and the existing
retry/resume machinery:

| Situation                        | error_type             | category             | recoverable |
|----------------------------------|------------------------|----------------------|-------------|
| walltime exceeded                | `slurm_timeout`        | `resource_error`     | yes         |
| out of memory                    | `slurm_out_of_memory`  | `resource_error`     | yes         |
| node/boot failure, preemption    | `slurm_node_failure` / `slurm_preempted` | `environment_error` | yes |
| invalid partition/QoS/account    | `slurm_submit_failed`  | `environment_error`  | no          |
| queue/QoS limits at submit       | `slurm_submit_failed`  | `resource_error`     | yes         |
| job vanished, no accounting      | `slurm_job_lost`       | `resource_error`     | yes         |
| `m3flow run cancel`              | `cancelled`            | `environment_error`  | no (`scancel`) |

Provider failures (engine crash, validation) are reported exactly as with
local execution — the executor is transport, not semantics.

## Limitations

- No job arrays (each task is an individual job), no remote-over-SSH driver
  (m3flow must run on the login node), no per-step partition/QoS overrides.
- Project paths must not contain spaces (`#SBATCH` directive values are not
  shell-quoted).
