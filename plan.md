# M3Flow：面向 Coding Agent 的分子动力学工作流平台完整开发设计

## 1. 项目定位

M3Flow 是一个面向分子模拟的 **task-centric、artifact-driven、provenance-first scientific workflow runtime**。

它不内置 AI Agent，而是为 Claude Code、pi-agent、Codex 等 coding agent，以及直接使用 CLI/TUI 的科研人员，提供一个稳定、可发现、可组合、可追踪的分子模拟执行环境。

核心定位：

> **M3Flow is an execution substrate between coding agents and molecular simulation software.**

它不负责理解科研问题本身，也不负责替用户决定科学策略，而负责：

- 定义规范化的科学原子操作；
- 将原子操作组合成科学工作流；
- 管理计算状态和科学数据；
- 复用已有平衡体系、生产轨迹和分析结果；
- 调用 AutoPoly、LAMMPS、OpenMM 等外部科学软件；
- 提供统一 CLI；
- 提供结构化错误和验证；
- 保存完整 provenance；
- 支持 workflow、state、trajectory 和 result 的复用。

---

# 2. 核心设计哲学

M3Flow 的两个核心原则是：

> **Task-centric execution**

以及：

> **Artifact-driven reuse**

其中：

> **A Task is one scientifically meaningful operation with a well-defined input/output contract.**

Task 是科学意义上的原子操作，而不是代码函数。

例如：

```text
build_system
parameterize_system
pack_system

energy_minimize
run_nvt
run_npt
run_nve

compute_density
compute_msd
compute_rdf
fit_diffusion
```

Workflow 是 Task 和其他 Workflow 的组合。

Artifact 是不同 Task 之间传递和复用的科学对象。

---

# 3. 系统目标

M3Flow 主要解决五个问题。

## 3.1 Workflow 可组合

例如：

```text
AutoPoly
   ↓
polymer equilibration
   ↓
production
   ↓
analysis
```

所有阶段均可独立复用。

---

## 3.2 计算结果可复用

同一个平衡体系可以支持多个 production：

```text
                 EquilibratedState
                        │
             ┌──────────┼──────────┐
             ▼          ▼          ▼
        NPT production  Dynamics   T sweep
             │          │          │
             ▼          ▼          ▼
          density    diffusion     CTE
```

同一个 production trajectory 也可以服务多个分析：

```text
ProductionTrajectory
      │
      ├── density
      ├── RDF
      ├── Rg
      ├── Ree
      ├── orientation
      └── MSD
```

---

## 3.3 Workflow 可追溯

任何结果都可以向上追踪：

```text
DiffusionResult
      ↑
fit_diffusion
      ↑
MSD
      ↑
ProductionTrajectory
      ↑
run_nvt
      ↑
EquilibratedState
      ↑
polymer_21step
      ↑
SimulationSystem
      ↑
AutoPoly
      ↑
SystemSpec
```

---

## 3.4 Coding agent 可以自然操作

Claude Code 不需要专用 SDK。

只需要：

```text
shell
filesystem
YAML
JSON
Git
```

即可：

```text
discover
→ inspect
→ compose
→ validate
→ run
→ inspect
→ repair
→ rerun
```

---

## 3.5 科学软件保持独立

AutoPoly、LAMMPS、OpenMM、GPUMD、Python analysis 等均保持独立。

M3Flow 只是：

```text
contracts
composition
execution
artifact management
validation
provenance
```

---

# 4. 总体架构

```text
┌───────────────────────────────────────────────┐
│             Human / Coding Agent              │
│ Claude Code / pi-agent / Codex / Shell / TUI  │
└───────────────────────┬───────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────┐
│                    CLI                        │
│ task / workflow / run / artifact / schema     │
└───────────────────────┬───────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────┐
│              Workflow Frontend                │
│ parser / schema / type checker / planner      │
└───────────────────────┬───────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────┐
│                 Workflow IR                   │
└───────────────────────┬───────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────┐
│              Workflow Runtime                 │
│ scheduler / state / cache / resume / retry    │
└─────────────┬────────────────┬────────────────┘
              │                │
              ▼                ▼
       Task Registry      Artifact Store
              │                │
              ▼                ▼
          Providers       Provenance DB
              │
      ┌───────┼────────────┐
      ▼       ▼            ▼
   AutoPoly LAMMPS      Analysis
              │
              ▼
          Executors
              │
      ┌───────┼─────────┐
      ▼       ▼         ▼
    Local   Slurm    Container
```

---

# 5. 核心领域模型

平台核心一等对象：

```text
TaskSpec
WorkflowSpec
WorkflowRun
TaskRun

Artifact
SystemSpec

Provider
Executor
```

Artifact 再按科学含义分为四类：

```text
System
State
Dataset
Result
```

对应：

```text
System
  ↓ simulation
State
  ↓ sampling
Dataset
  ↓ analysis
Result
```

这是 M3Flow 最重要的数据流模型。

---

# 6. TaskSpec

## 6.1 定义

Task 必须满足：

- 一个明确科学操作；
- 输入明确；
- 输出明确；
- 参数明确；
- 可以独立验证；
- 可以重复使用；
- 与具体 workflow 解耦。

例如：

```text
energy_minimize
run_npt
compute_density
compute_msd
```

不是：

```text
set_temperature
write_input
simulate_everything
calculate_all_properties
```

---

## 6.2 TaskSpec 示例

```yaml
schema: task/v1

name: run_npt
version: 1.0.0

description: >
  Run molecular dynamics in an NPT ensemble.

category: simulation

tags:
  - md
  - npt
  - thermodynamics

inputs:
  state:
    type: SimulationState
    required: true

parameters:
  temperature:
    type: temperature
    required: true

  pressure:
    type: pressure
    default: 1 bar

  timestep:
    type: time
    default: 1 fs

  duration:
    type: time
    required: true

outputs:
  state:
    type: SimulationState

  trajectory:
    type: Trajectory

  log:
    type: SimulationLog

validation:
  - simulation_completed
  - no_nan
  - no_lost_atoms
  - trajectory_readable

implementations:
  - provider: lammps
  - provider: openmm
```

---

# 7. Artifact Model

Artifact 是平台真正实现复用的基础。

每个 Artifact：

```text
Artifact
├── id
├── type
├── schema_version
├── files
├── metadata
├── content_hash
├── producer
├── created_at
└── provenance
```

例如：

```json
{
  "id": "traj_8f31",
  "type": "ProductionTrajectory",
  "schema_version": "1",
  "files": {
    "trajectory": "production.xtc",
    "log": "production.log"
  },
  "metadata": {
    "ensemble": "NPT",
    "temperature": "300 K",
    "pressure": "1 bar",
    "duration": "5 ns",
    "sampling_interval": "1 ps"
  },
  "producer": "taskrun_17b2"
}
```

---

# 8. Artifact 分类

## 8.1 System

描述“模拟对象是什么”。

```text
MolecularSystem
ParameterizedSystem
SimulationSystem
```

---

## 8.2 State

描述“体系当前处于什么状态”。

```text
SimulationState
EquilibratedState
```

`EquilibratedState` 应保存：

```text
coordinates
velocities
box
topology
force field
checkpoint/restart
temperature
pressure
equilibration protocol
validation metrics
provenance
```

这是高分子 workflow 中极其重要的可复用资产。

---

## 8.3 Dataset

描述“从 simulation 中采样到了什么”。

```text
Trajectory
ProductionTrajectory
ThermodynamicSeries
TemperatureSeries
StressStrainSeries
```

---

## 8.4 Result

科学分析结果：

```text
DensityResult
DiffusionResult
CTEResult
RDFResult
TgResult
AdhesionResult
```

---

# 9. SystemSpec

SystemSpec 是 Workflow Platform 和 AutoPoly 的核心接口。

不分别为：

```text
liquid
polymer
interface
CG
```

设计完全不同的数据结构。

而采用三个正交维度：

```text
SystemSpec
├── Composition
├── Environment
└── Resolution
```

---

# 10. Composition

支持：

```text
molecule
polymer
substrate
particle
```

例如：

```yaml
components:

  - id: peo

    type: polymer

    representation:
      type: psmiles
      value: "[*]CCO[*]"

    topology: linear

    degree_of_polymerization: 50

    number_of_chains: 20
```

液体：

```yaml
components:

  - id: ethanol

    type: molecule

    representation:
      type: smiles
      value: CCO

    count: 200
```

---

# 11. Environment

支持：

```text
isolated
bulk
mixture
interface
film
confined
```

例如：

```yaml
environment:
  type: bulk
  target_density: 0.85 g/cm3
```

界面：

```yaml
environment:

  type: interface

  normal: z

  lower: silica
  upper: polymer

  gap: 3 angstrom
```

---

# 12. Resolution

```yaml
resolution:
  type: atomistic
```

或者：

```yaml
resolution:
  type: coarse_grained
  model: custom
```

因此：

```text
organic liquid
= molecule + bulk + atomistic

polymer melt
= polymer + bulk + atomistic

polymer/silica interface
= polymer + substrate + interface + atomistic

CG polymer melt
= polymer + bulk + coarse_grained
```

---

# 13. AutoPoly 的系统角色

AutoPoly 定位：

> **Molecular System Construction Engine**

职责：

```text
SystemSpec
    ↓
geometry
    ↓
parameterization
    ↓
packing / assembly
    ↓
SimulationSystem
```

AutoPoly 当前已有：

```text
organic molecule
liquid
polymer
interface
coarse-grained system
```

因此非常适合成为 M3Flow 第一个 system construction provider。

---

# 14. AutoPoly Integration

采用：

> **Adapter-first, evolve-later**

第一阶段不大规模修改 AutoPoly。

M3Flow：

```text
Task
 ↓
AutoPolyProvider
 ↓
existing AutoPoly API
 ↓
existing output
 ↓
typed Artifact
```

建议暴露：

```text
build_system
parameterize_system
prepare_simulation_system
```

内部映射现有 AutoPoly 能力。

未来真实 workflow 缺什么，再扩展 AutoPoly。

---

# 15. WorkflowSpec

Workflow 是 Task 和 Workflow 的组合。

例如：

```yaml
schema: workflow/v1

name: polymer_density
version: 1.0.0

inputs:
  system:
    type: SystemSpec

steps:

  build:
    task: build_system

    inputs:
      specification: ${inputs.system}

  equilibrate:
    workflow: polymer_21step_equilibration

    inputs:
      system: ${build.system}

  production:
    workflow: npt_thermodynamic_production

    inputs:
      state: ${equilibrate.state}

  density:
    task: compute_density

    inputs:
      trajectory: ${production.trajectory}

outputs:

  density:
    value: ${density.result}
```

---

# 16. Workflow 分层

建议 Workflow Library 分四层。

## Layer 1 — Atomic Tasks

```text
build_system
parameterize_system

minimize
run_nvt
run_npt

compute_density
compute_msd
fit_diffusion
```

---

## Layer 2 — Preparation Protocols

```text
polymer_21step_equilibration
simple_npt_equilibration
cg_push_off
interface_relaxation
backmapping_relaxation
```

---

## Layer 3 — Sampling / Production Protocols

这是整个架构的重要组成部分：

```text
npt_thermodynamic_production
dynamics_production
temperature_sweep
mechanical_deformation
interface_separation
```

---

## Layer 4 — Property / Scientific Workflows

```text
density
diffusion
RDF
CTE
Tg
adhesion
mechanical_properties
```

---

# 17. 21-Step Polymer Equilibration

21-step equilibration 是：

```text
Workflow
```

不是 Task。

底层使用：

```text
minimize
run_nvt
run_npt
```

等 atomic Task。

例如：

```text
SimulationSystem
      ↓
minimize
      ↓
NVT
      ↓
NPT
      ↓
...
      ↓
step 21
      ↓
check_equilibration
      ↓
EquilibratedState
```

---

# 18. 21-Step Protocol 应版本化

例如：

```text
polymer_21step_equilibration@1.0.0
```

记录：

```text
protocol version
temperature schedule
pressure schedule
duration
timestep
ensemble sequence
literature reference
validation criteria
```

Workflow 应尽可能 immutable。

修改协议：

```text
fork → new workflow version
```

而不是偷偷改原协议。

---

# 19. Protocol 静态展开

21 steps 是已知的，所以建议：

```text
compile-time expansion
```

而不是 runtime loop。

源码可以：

```yaml
stages:

  - ensemble: nvt
    temperature: 300 K
    duration: 50 ps

  - ensemble: npt
    temperature: 300 K
    pressure: 1000 atm
    duration: 50 ps
```

编译成：

```text
21 explicit WorkflowNodes
```

这样 provenance 更清楚。

---

# 20. Equilibration Validation

执行完 21 steps 不应自动等价于 scientifically equilibrated。

建议增加：

```text
check_polymer_equilibration
```

输入：

```text
trajectory
state
log
system
```

输出：

```text
EquilibrationReport
```

可能包括：

```text
density drift
energy stationarity
Rg stability
Ree stability
box-volume stability
```

得到：

```json
{
  "equilibrated": true,
  "metrics": {
    "density_drift": 0.0012,
    "energy_stationary": true,
    "rg_stationary": true
  }
}
```

---

# 21. Adaptive Equilibration

可以进一步提供：

```text
equilibrate_polymer
```

作为高级 Workflow：

```text
21-step protocol
      ↓
check_equilibration
      ↓
   converged?
     /   \
   yes    no
    │      │
    ▼      ▼
 output   extend NPT
             ↓
          check again
```

这是 runtime control flow，不是 AI。

---

# 22. Production 与 Property 的解耦

M3Flow 不采用：

```text
calculate_density
  ↓
equilibrate
  ↓
production
  ↓
density
```

作为唯一结构。

而采用：

```text
EquilibratedState
       ↓
Production protocol
       ↓
Dataset
       ↓
Property analysis
```

因此 production 是可以共享的。

---

# 23. EquilibratedState Fan-out

例如：

```text
                     EquilibratedState
                           │
              ┌────────────┼──────────────┐
              │            │              │
              ▼            ▼              ▼
       NPT Production  Dynamics Prod.  T Sweep
              │            │              │
              ▼            ▼              ▼
      ThermoTrajectory DynTrajectory   T-V Series
         │    │    │        │              │
         ▼    ▼    ▼        ▼              ▼
      density RDF  Rg      MSD            CTE
                            ↓
                        Diffusion
```

---

# 24. 同一 Production 的多个分析

例如：

```text
NPT ProductionTrajectory
        │
        ├── density
        ├── RDF
        ├── Rg
        ├── Ree
        └── structure analysis
```

这些 analysis Task 不需要重新跑 production。

---

# 25. Diffusion

Diffusion 的 workflow：

```text
EquilibratedState
      ↓
Dynamics Production
      ↓
ProductionTrajectory
      ↓
compute_msd
      ↓
MSDResult
      ↓
fit_diffusion
      ↓
DiffusionResult
```

如果已有 compatible trajectory：

```text
skip simulation
```

直接：

```text
compute_msd
→ fit_diffusion
```

---

# 26. Density

例如：

```text
EquilibratedState
      ↓
NPT thermodynamic production
      ↓
ProductionTrajectory
      ↓
compute_density
```

如果已有合适 NPT trajectory：

```text
directly reuse
```

---

# 27. Thermal Expansion Coefficient

CTE 不是单 trajectory property。

需要：

```text
EquilibratedState
      ↓
temperature sweep
      ↓
ThermodynamicStateSeries
      ↓
V(T) / ρ(T)
      ↓
fit_cte
      ↓
CTEResult
```

例如：

```text
250 K
275 K
300 K
325 K
350 K
```

可以 sequential annealing，也可以从同一个 reference state 分支。

具体由 protocol 决定。

---

# 28. ProductionTrajectory 的语义

Trajectory 不只应该是“轨迹文件”。

它需要 metadata：

```yaml
type: ProductionTrajectory

ensemble: NPT

temperature: 300 K
pressure: 1 bar

duration: 10 ns

timestep: 1 fs

sampling_interval: 1 ps

observables:
  - coordinates
  - box
  - energy
  - pressure
```

这样它才可以被自动判断是否适合下游 Task。

---

# 29. Task Data Requirements

Property Task 应声明需求。

例如：

```yaml
name: compute_density

inputs:
  trajectory:
    type: ProductionTrajectory

requirements:
  ensemble:
    - NPT

  observables:
    - box
```

Diffusion：

```yaml
requirements:

  observables:
    - coordinates

  needs_time_axis: true

  dynamics_sensitive: true
```

CTE：

```yaml
inputs:
  series:
    type: ThermodynamicStateSeries

requirements:
  varying:
    - temperature

  observables:
    - volume
```

---

# 30. Artifact Compatibility

平台增加：

```bash
M3Flow artifact compatible \
    TRAJECTORY_ID compute_density
```

返回：

```json
{
  "compatible": true
}
```

或者：

```json
{
  "compatible": false,
  "missing": [
    "volume",
    "temperature sweep"
  ]
}
```

注意：

M3Flow 只做 contract checking。

不做 scientific reasoning。

---

# 31. Artifact Reuse

M3Flow 的 workflow planner 在运行前搜索：

```text
已有 output artifact?
```

例如：

```text
build                CACHED
parameterization     CACHED
equilibration        CACHED
production           CACHED

compute_density      RUN
```

因此 cache 不只是性能优化。

它是：

> scientific state reuse system

---

# 32. Cache Key

Task cache key：

```text
hash(
  TaskSpec version
  implementation version
  input artifact hashes
  parameters
  relevant environment
)
```

Workflow 自己不 cache。

Workflow 的每一个 Task independently cache。

这样局部修改 workflow 时，只重新运行受影响节点。

---

# 33. Provenance

最基本模型：

```text
Artifact ──used_by──> TaskRun

TaskRun ──generated──> Artifact
```

WorkflowRun 是 TaskRun 的容器。

---

# 34. Provenance Example

```text
SystemSpec
   ↓
AutoPoly TaskRun
   ↓
SimulationSystem
   ↓
Equilibration WorkflowRun
   ↓
EquilibratedState
   ↓
Production WorkflowRun
   ↓
ProductionTrajectory
   ↓
MSD TaskRun
   ↓
MSDResult
   ↓
Fit TaskRun
   ↓
DiffusionResult
```

---

# 35. Workflow Runtime

Runtime 负责：

```text
dependency resolution
type checking
execution
state transitions
cache
resume
retry
failure handling
artifact registration
provenance
```

状态：

```text
PENDING
READY
RUNNING
COMPLETED
FAILED
CACHED
SKIPPED
CANCELLED
```

---

# 36. Workflow IR

Workflow YAML 不能直接执行。

过程：

```text
Workflow YAML
    ↓
Parser
    ↓
Schema validation
    ↓
Workflow IR
    ↓
Type checking
    ↓
Dependency resolution
    ↓
Artifact resolution
    ↓
Execution plan
```

WorkflowNode：

```text
id
task/workflow ref
input bindings
parameters
dependencies
condition
retry policy
metadata
```

---

# 37. 控制流

v0.1：

```text
sequence
dependency
fan-out
fan-in
retry
```

v0.2：

```text
condition
foreach
static expansion
```

v0.3：

```text
while
dynamic loop
adaptive protocol
```

---

# 38. Provider Architecture

Provider 是科学工具接入层。

统一接口：

```text
describe
validate
execute
diagnose
```

第一批：

```text
AutoPolyProvider
LAMMPSProvider
PythonAnalysisProvider
```

未来：

```text
OpenMMProvider
GPUMDProvider
CP2KProvider
MACEProvider
```

---

# 39. Provider 与 Runtime 隔离

建议 provider 独立 process。

例如：

```bash
M3Flow-autopoly execute request.json
```

返回：

```json
{
  "status": "success",
  "outputs": {
    "system": "artifact.json"
  }
}
```

这样：

```text
Rust core
↔ JSON/process protocol
↔ Python provider
```

不同语言完全解耦。

---

# 40. Executor

Provider 负责“怎么生成命令”。

Executor 负责“在哪里执行”。

第一阶段：

```text
LocalExecutor
ProcessExecutor
```

之后：

```text
ContainerExecutor
SSHExecutor
SlurmExecutor
```

未来：

```text
Kubernetes
Cloud
```

---

# 41. HPC 资源声明

Task 可以声明：

```yaml
resources:

  cpu: 8

  gpu: 1

  memory: 16 GB

  walltime: 2 h
```

Workflow runtime 不关心具体 Slurm syntax。

---

# 42. CLI

CLI 应作为第一优先级接口。

建议：

```bash
M3Flow
```

---

## 42.1 Task Discovery

```bash
M3Flow task list
M3Flow task search polymer
M3Flow task inspect run_npt
M3Flow task inspect run_npt --json
```

---

## 42.2 Workflow

```bash
M3Flow workflow list
M3Flow workflow inspect polymer_21step

M3Flow workflow validate workflow.yaml
M3Flow workflow plan workflow.yaml

M3Flow workflow run workflow.yaml
M3Flow workflow run workflow.yaml --dry-run
```

---

## 42.3 Run

```bash
M3Flow run list
M3Flow run inspect RUN_ID
M3Flow run logs RUN_ID
M3Flow run graph RUN_ID
M3Flow run resume RUN_ID
M3Flow run retry RUN_ID TASK_ID
```

---

## 42.4 Artifact

```bash
M3Flow artifact list
M3Flow artifact inspect ARTIFACT_ID
M3Flow artifact lineage ARTIFACT_ID

M3Flow artifact compatible \
    ARTIFACT_ID compute_density
```

---

## 42.5 Schema

```bash
M3Flow schema list

M3Flow schema show SystemSpec

M3Flow schema show ProductionTrajectory
```

---

# 43. JSON Everywhere

所有命令支持：

```text
--json
```

例如：

```bash
M3Flow workflow validate workflow.yaml --json
```

输出：

```json
{
  "valid": false,
  "errors": [
    {
      "type": "input_type_mismatch",
      "step": "diffusion",
      "expected": "ProductionTrajectory",
      "received": "SimulationSystem"
    }
  ]
}
```

Coding agent 永远不依赖 regex 解析 human terminal output。

---

# 44. Error Model

统一：

```text
SchemaError
TypeError
WorkflowError
TaskError
ProviderError
ExecutionError
ScientificValidationError
ArtifactCompatibilityError
```

LAMMPS lost atoms：

```json
{
  "error_type": "lost_atoms",
  "category": "simulation_instability",
  "recoverable": true,
  "provider": "lammps",
  "task": "run_npt",
  "details": {
    "original_atoms": 5000,
    "remaining_atoms": 4992
  },
  "raw_log": "artifact://log_1931"
}
```

平台不直接建议科学修复策略。

外部 coding agent 自己决定。

---

# 45. Scientific Validation

Task success 分成：

```text
execution success
scientific validation success
```

例如：

```text
LAMMPS exit = 0
```

不一定说明：

```text
simulation scientifically usable
```

因此每个 scientific Task 可定义 validator。

---

# 46. TUI

TUI 定位：

> execution cockpit

而不是大型 workflow GUI。

例如：

```text
Polymer Properties

✓ AutoPoly build
✓ parameterize
✓ 21-step equilibration
✓ validation

├─ ▶ NPT production     65%
│    ├─ ○ density
│    ├─ ○ RDF
│    └─ ○ Rg
│
├─ ▶ Dynamics production
│    └─ ○ diffusion
│
└─ ○ Temperature sweep
     └─ ○ CTE
```

快捷键：

```text
[l] logs
[a] artifacts
[p] provenance
[g] graph
[r] retry
```

---

# 47. Project Structure

```text
project/

├── systems/
│   ├── peo.yaml
│   └── ps.yaml

├── workflows/
│   ├── equilibrate.yaml
│   ├── production.yaml
│   └── properties.yaml

├── results/

├── M3Flow.yaml

└── .M3Flow/
    ├── runs/
    ├── artifacts/
    ├── cache/
    └── M3Flow.db
```

适合 Claude Code：

```text
read
→ edit
→ validate
→ run
→ inspect
→ modify
```

---

# 48. Workflow Registry

建议组织为：

```text
workflows/

├── preparation/
│   ├── polymer_21step
│   ├── simple_equilibration
│   └── cg_push_off

├── production/
│   ├── npt_thermodynamic
│   ├── dynamics
│   ├── temperature_sweep
│   └── deformation

└── properties/
    ├── density
    ├── diffusion
    ├── rdf
    ├── cte
    ├── tg
    └── adhesion
```

---

# 49. Reusable Property Workflow Example

可以设计：

```yaml
name: polymer_basic_properties

inputs:
  equilibrated_state:
    type: EquilibratedState

steps:

  thermo:
    workflow: npt_thermodynamic_production

    inputs:
      state: ${inputs.equilibrated_state}

  density:
    task: compute_density

    inputs:
      trajectory: ${thermo.trajectory}

  rdf:
    task: compute_rdf

    inputs:
      trajectory: ${thermo.trajectory}

  dynamics:
    workflow: dynamics_production

    inputs:
      state: ${inputs.equilibrated_state}

  msd:
    task: compute_msd

    inputs:
      trajectory: ${dynamics.trajectory}

  diffusion:
    task: fit_diffusion

    inputs:
      msd: ${msd.result}
```

这体现了真正的 artifact reuse。

---

# 50. Production Protocol Profiles

Production 应成为 reusable protocol。

例如：

```text
production/npt_thermodynamic@1
production/dynamics@1
production/temperature_sweep@1
```

不同 scientific workflow 可以复用。

---

# 51. Scientific Protocol Metadata

WorkflowSpec 建议增加：

```yaml
kind: scientific_protocol

domain:
  - polymer
  - molecular_dynamics

purpose:
  - equilibration

applicability:
  systems:
    - amorphous_polymer

references:
  - doi: ...

assumptions:
  - periodic_boundary
  - classical_md
```

这样 workflow 本身成为可审计 scientific asset。

---

# 52. Versioning

必须版本化：

```text
TaskSpec
WorkflowSpec
Artifact schema
Provider
Implementation
```

例如：

```text
run_npt@1.2.0

polymer_21step@1.1.0

ProductionTrajectory/v1
```

---

# 53. Reproducibility Metadata

每个 TaskRun 保存：

```text
task version
provider version
git commit

input artifact hashes
parameters
random seed

software versions
engine version

container image
environment

hostname
hardware

start/end time

stdout/stderr

scientific validation
```

---

# 54. Git Integration

Workflow 和 SystemSpec 都是普通文本。

因此：

```text
git commit
git diff
git branch
code review
```

全部自然支持。

Run 可以保存：

```text
workflow_git_commit
dirty_worktree
workflow_hash
system_spec_hash
```

---

# 55. Storage

MVP：

```text
SQLite + filesystem
```

完全够用。

数据库：

```text
workflow_run
task_run
artifact
artifact_input
artifact_output
workflow_dependency
metadata
```

文件：

```text
.M3Flow/artifacts/
```

---

# 56. Content-addressable Artifact Store

建议：

```text
artifacts/
└── sha256/
    ├── ab/
    ├── 42/
    └── ...
```

可以自然实现：

```text
deduplication
cache
integrity verification
reproducibility
```

---

# 57. 推荐代码架构

核心 runtime 可以使用 Rust。

```text
M3Flow/

├── crates/

│   ├── core/
│   │   ├── task
│   │   ├── artifact
│   │   ├── system
│   │   ├── workflow
│   │   └── provenance

│   ├── runtime/
│   │   ├── scheduler
│   │   ├── state
│   │   ├── executor
│   │   ├── cache
│   │   └── resolver

│   ├── registry/
│   │   ├── task_registry
│   │   ├── workflow_registry
│   │   └── provider_registry

│   ├── cli/

│   └── tui/

├── providers/

│   ├── autopoly/
│   ├── lammps/
│   └── analysis/

├── schemas/

│   ├── task.schema.json
│   ├── workflow.schema.json
│   ├── artifact.schema.json
│   └── system.schema.json

├── workflows/

│   ├── preparation/
│   ├── production/
│   └── properties/

└── examples/
```

---

# 58. 为什么核心 runtime 适合 Rust

优势：

```text
single binary
strong typing
fast CLI
safe concurrency
process control
good TUI ecosystem
predictable deployment
```

但：

```text
AutoPoly → Python
analysis → Python
LAMMPS → binary
```

都不需要重写。

Provider protocol 将语言完全隔离。

---

# 59. 第一阶段 MVP

只实现最小 vertical slice。

## Core

```text
TaskSpec
Artifact
WorkflowSpec
Workflow IR
Registry
LocalExecutor
SQLite provenance
Filesystem artifact store
CLI
```

## Provider

```text
AutoPoly
LAMMPS
Python analysis
```

## Task

```text
build_system
parameterize_system
prepare_simulation_system

energy_minimize
run_nvt
run_npt

compute_density
```

---

# 60. MVP Reference Workflow

第一条：

```text
PEO SystemSpec
      ↓
AutoPoly
      ↓
SimulationSystem
      ↓
21-step equilibration
      ↓
EquilibratedState
      ↓
NPT production
      ↓
ProductionTrajectory
      ↓
DensityResult
```

必须做到：

```text
workflow editable
typed I/O
cache
artifact reuse
run provenance
structured errors
JSON CLI
```

---

# 61. 第二个 Reference Workflow

```text
Organic liquid diffusion
```

测试：

```text
molecular liquid
equilibration
dynamics production
MSD
diffusion
```

---

# 62. 第三个 Reference Workflow

```text
Polymer multi-property workflow
```

从一个 EquilibratedState：

```text
NPT production
├── density
├── RDF
└── Rg

Dynamics production
└── diffusion

Temperature sweep
└── CTE
```

这是检验 artifact reuse 的核心 benchmark。

---

# 63. 第四个 Reference Workflow

```text
Polymer–silica interface adhesion
```

测试：

```text
AutoPoly interface
multi-component artifact
equilibration
production
interface analysis
```

---

# 64. 第五个 Reference Workflow

```text
CG polymer melt
```

用于验证：

```text
resolution abstraction
```

是否真正和 atomistic workflow 解耦。

---

# 65. 开发阶段

## Phase 0 — Specification

先确定：

```text
TaskSpec
WorkflowSpec
ArtifactSpec
SystemSpec
Provider protocol
```

不写复杂 runtime。

---

## Phase 1 — Kernel

实现：

```text
parser
validator
type checker
registry
artifact store
task execution
provenance
CLI
```

---

## Phase 2 — AutoPoly

打通：

```text
SystemSpec
↓
AutoPolyProvider
↓
SimulationSystem
```

---

## Phase 3 — LAMMPS

实现：

```text
minimize
NVT
NPT
```

---

## Phase 4 — Polymer Equilibration

实现：

```text
polymer_21step
check_equilibration
EquilibratedState
```

---

## Phase 5 — Production / Sampling

实现：

```text
npt_thermodynamic
dynamics
temperature_sweep
```

---

## Phase 6 — Property Analysis

实现：

```text
density
RDF
Rg
MSD
diffusion
CTE
```

---

## Phase 7 — Artifact Reuse

实现：

```text
cache lookup
compatibility checking
fan-out
reuse existing state
reuse trajectory
```

这是平台开始真正体现优势的阶段。

---

## Phase 8 — TUI

实现 execution cockpit。

---

## Phase 9 — HPC

实现：

```text
SlurmExecutor
resources
queue status
resume
```

---

## Phase 10 — Workflow Library

形成：

```text
preparation/
production/
properties/
```

标准 scientific workflow library。

---

# 66. Coding Agent Benchmark

M3Flow 最值得建立一个自己的 usability benchmark。

测试：

只给 Claude Code：

```bash
M3Flow --help
```

然后要求：

> Build a PEO melt, equilibrate it, calculate density and diffusion at 300 K.

看 coding agent 是否能：

```text
discover tasks
inspect schemas
construct SystemSpec
construct WorkflowSpec
validate
execute
inspect failure
repair
reuse equilibrium
reuse production
extract results
```

---

# 67. Agent-Friendly Design Checklist

每个新 feature 都问：

```text
Can it be discovered from CLI?

Can the schema be inspected?

Does it support --json?

Does failure return structured diagnostics?

Can a coding agent modify it as text?

Can it be version controlled?

Can its output be reused?

Can its provenance be inspected?
```

如果不能，优先修接口，而不是加更多功能。

---

# 68. Non-goals

平台第一阶段明确不做：

```text
built-in LLM
planner agent
multi-agent framework
web GUI
autonomous scientific decisions
automatic literature reasoning
automatic force-field selection
automatic scientific protocol selection
workflow marketplace
large cloud platform
```

M3Flow 保持：

> small, deterministic, inspectable core.

---

# 69. 最重要的科学边界

M3Flow 不决定：

```text
Which equilibration protocol is scientifically correct?

Should diffusion use NVT or NVE?

Is 5 ns sufficient?

Which Tg protocol should be used?
```

M3Flow 负责：

```text
What protocol was requested?

Are the input requirements satisfied?

Can existing artifacts be reused?

Did execution succeed?

Did declared validation pass?

Where did every result come from?
```

科学决策由：

```text
human
or
external coding agent
```

完成。

---

# 70. 最终总体模型

最终 M3Flow 可以概括为：

```text
                   Scientific Intent
                         │
                         ▼
                Human / Coding Agent
                         │
                         ▼
                   WorkflowSpec
                         │
                         ▼
┌──────────────────────────────────────────┐
│                 M3Flow                  │
│                                          │
│ TaskSpec                                 │
│ Workflow IR                              │
│ Artifact Model                           │
│ Runtime                                  │
│ Registry                                 │
│ Cache                                    │
│ Provenance                               │
└─────────────────┬────────────────────────┘
                  │
          ┌───────┼───────────┐
          ▼       ▼           ▼
       AutoPoly LAMMPS      Analysis
          │       │           │
          ▼       ▼           ▼
       System    State      Dataset
                  │
                  ▼
                Result
```

科学计算层面则是：

```text
SystemSpec
    ↓
System Construction
    ↓
SimulationSystem
    ↓
Preparation / Equilibration
    ↓
EquilibratedState
    ↓
Sampling / Production
    ↓
Dataset
    ↓
Analysis
    ↓
Scientific Result
```

---

# 71. 最终核心原则

整个架构最终可以浓缩成六句话：

1. **Task 是科学原子操作。**
2. **Workflow 是 Task 和 Workflow 的组合。**
3. **Artifact 是 workflow 之间真正的接口。**
4. **EquilibratedState 和 ProductionTrajectory 必须可以复用。**
5. **Platform 不内置 Agent，但一切接口都应方便 coding agent 操作。**
6. **Runtime executes scientific protocols; it does not define scientific truth.**

如果这六点一直保持不变，M3Flow 后面即使扩展到 AutoPoly、LAMMPS、OpenMM、GPUMD、MLIP、CP2K、粗粒化、界面、电子封装材料以及更复杂的多尺度 workflow，核心架构都不需要推翻。