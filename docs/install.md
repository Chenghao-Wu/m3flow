# Installation

M3Flow has three installable pieces, deliberately decoupled:

1. **The `m3flow` binary** — the runtime, CLI, and TUI (Rust, single static
   executable).
2. **The providers** — Python processes (`m3flow-autopoly`, `m3flow-lammps`,
   `m3flow-analysis`) speaking the `m3flow-provider/1` JSON protocol on PATH.
3. **The engines** — external simulation software (LAMMPS, AutoPoly) that the
   providers drive. Engines are external on purpose: M3Flow never bundles a
   simulation code.

## From a release (recommended)

### 1. The binary

Download the archive for your platform from
[GitHub Releases](https://github.com/Chenghao-Wu/m3flow/releases/latest):

| target | platform |
|---|---|
| `x86_64-unknown-linux-gnu` | recent Linux (glibc ≥ 2.39) |
| `x86_64-unknown-linux-musl` | fully static — older clusters, any distro |
| `x86_64-apple-darwin` | Intel Mac |
| `aarch64-apple-darwin` | Apple Silicon |
| `x86_64-pc-windows-msvc` | Windows (zip) |

```bash
tar xzf m3flow-<version>-<target>.tar.gz
sudo install m3flow-<version>-<target>/m3flow /usr/local/bin/
```

!!! tip "Clusters without internet-facing package managers"
    The `musl` build is fully static and runs on any x86-64 Linux regardless
    of glibc age — it is the right choice for HPC login nodes.

### 2. The providers

Released on the same tag train as the binary — the latest PyPI release matches
the latest binary release.

```bash
pip install m3flow-providers
```

or download the `.whl` from the release page and `pip install` that file.

### 3. The engines

Install engines into the providers' Python environment or onto PATH:

=== "LAMMPS"

    ```bash
    mamba install -c conda-forge lammps
    ```

    or point at an existing binary in `m3flow.yaml`:

    ```yaml
    providers:
      lammps:
        engine:
          executable: /path/to/lmp
    ```

=== "AutoPoly"

    ```bash
    pip install git+https://github.com/WuGroup-XJTLU/AutoPoly.git
    ```

    !!! warning
        The AutoPoly `0.0.1` on PyPI predates the current API — install from
        the GitHub repository.

## Conda

A combined conda package (binary + providers in one) is planned for
conda-forge. The rattler-build recipe lives in `conda/` and can be built
locally meanwhile:

```bash
rattler-build build -r conda/recipe.yaml -m conda/variants-local.yaml \
    --output-dir conda/out
mamba install -c conda/out m3flow
```

## From source

Requires a Rust toolchain and Python ≥ 3.10:

```bash
git clone https://github.com/Chenghao-Wu/m3flow
cd m3flow
cargo build --release          # binary: target/release/m3flow
pip install -e providers/      # m3flow-autopoly, m3flow-lammps, m3flow-analysis
```

## Verify the install

```bash
m3flow provider list     # all providers report "ok"
m3flow task list         # the 19 atomic tasks
m3flow workflow list     # the 18 library workflows
```

If a provider does not report `ok`, run `m3flow provider diagnose <name>` —
it performs environment checks and tells you exactly what is missing
(typically an engine executable).

## Project configuration

`m3flow init` creates a project directory with a machine-local `m3flow.yaml`
(provider paths, engine executables, Slurm settings). This file is
machine-local on purpose and is not committed to version control; see
[Running on Slurm](slurm.md) for the executor configuration keys.
