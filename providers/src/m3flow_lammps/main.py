"""m3flow-lammps: MD tasks via LAMMPS (deck generation, log parsing, validation).

Implements: energy_minimize, run_nvt, run_npt, run_nve, run_soft_pushoff,
run_deform (tasks/lammps/*.yaml).

Conventions:
  - canonical quantities arrive as {value, unit} in canonical units
    (K, bar, fs); LAMMPS real units need atm for pressure (bar / 1.01325)
  - only `units real` and `units lj` systems are supported; anything else is
    rejected before a deck is written
  - `units lj` systems (CG) carry no physical calibration. Inputs use a
    fixed *convention*, not a mapping: 1 K == T* = 1, 1 bar == P* = 1 and
    1 tau == 1000 fs (so the duration/timestep ratio, i.e. the step count,
    is exact). Outputs of lj runs are labeled as reduced units
    (`*_lj` thermo columns, `tau` time axes) and never as physical units.
  - thermo output is never normalized per atom (`thermo_modify norm no`) so
    energy columns are extensive totals in every unit style
  - sampling metadata records the *actual* dump/thermo stride x timestep,
    not the requested interval (they differ when interval/dt is not integer)
  - state chaining: every run ends with write_data (coeffs + velocities) and
    write_restart; the next deck re-reads them
"""

from __future__ import annotations

import math
import os
import re
import shutil
import struct
import subprocess
from pathlib import Path

from m3flow_provider import (Provider, ProviderFailure, artifact, verdict)

PROVIDER_VERSION = "0.4.0"
ATM_PER_BAR = 1.0 / 1.01325
FS_PER_TAU = 1000.0  # lj-time convention for CG systems (not a calibration)
SUPPORTED_UNITS = ("real", "lj")
# LAMMPS pressure unit -> GPa ("real": atm). lj pressures stay reduced.
GPA_PER_LAMMPS_PRESSURE = {"real": 101325.0e-9}
THERMOSTATS = ("nose_hoover", "langevin", "berendsen")
BAROSTATS = ("nose_hoover", "berendsen")
CHECK_MARKER = "M3FLOW_CHECK_FAILED"
CHARGE_MARKER = "M3FLOW_GROUP_CHARGE"


# ------------------------------------------------------------------ engine

def _binary(req=None):
    cfg = (req or {}).get("config") or {}
    exe = cfg.get("executable") or cfg.get("lammps_executable")
    if exe and Path(exe).is_file():
        return exe
    found = shutil.which("lmp") or shutil.which("lammps")
    if found:
        return found
    default = "/home/zhenghaowu/lammps/build/lmp"
    if Path(default).is_file():
        return default
    raise ProviderFailure(
        "engine_missing", "environment_error",
        "LAMMPS binary not found (set providers.lammps.engine.executable in "
        "m3flow.yaml or put 'lmp' on PATH)")


def _engine(req=None):
    """Descriptor of the binary that `execute` would actually launch.

    Probes `_binary(req)` — the same resolution `_run_lammps` uses — so a
    configured executable is fingerprinted instead of whatever `lmp` is on
    PATH. The resolved path and compiled-in packages join the descriptor:
    the runtime hashes all of it into cache keys.
    """
    exe = _binary(req)
    out = subprocess.run([exe, "-h"], capture_output=True, text=True, timeout=60)
    text = out.stdout + out.stderr
    version = "unknown"
    for line in text.splitlines():
        if "Large-scale Atomic" in line:
            # "... Simulator - 22 Jul 2025 - Update 3"
            version = line.split("Simulator", 1)[-1].strip(" -") or line.strip()[:60]
            break
    return {"name": "lammps", "version": version,
            "executable": str(Path(exe).resolve()),
            "packages": sorted(_installed_packages(exe))}


_PKG_CACHE: dict = {}


def _installed_packages(exe):
    """LAMMPS packages compiled into `exe`, parsed from `lmp -h` (cached).

    Keyed on exe path + mtime so a rebuilt binary re-probes. Returns an
    empty frozenset when the probe fails — callers treat that as
    "no acceleration packages" rather than an error.
    """
    try:
        key = f"{exe}:{Path(exe).stat().st_mtime}"
    except OSError:
        key = exe
    if key not in _PKG_CACHE:
        try:
            out = subprocess.run([exe, "-h"], capture_output=True,
                                 text=True, timeout=60)
            text = out.stdout + out.stderr
        except (OSError, subprocess.TimeoutExpired):
            text = ""
        m = re.search(r"Installed packages:\s*\n+(.*?)(?:\n\s*\n|\Z)",
                      text, re.S)
        _PKG_CACHE.clear()  # only ever probe one active binary
        _PKG_CACHE[key] = frozenset(m.group(1).split()) if m else frozenset()
    return _PKG_CACHE[key]


# ------------------------------------------------------------------ context

class Ctx:
    def __init__(self, req):
        self.req = req
        self.params = req["parameters"]
        self.workdir = Path(req["workdir"])
        self.warnings = []
        # actual strides chosen by the deck builders (recorded in metadata)
        self.thermo_every = None
        self.dump_every = None
        self.n_steps = None
        inputs = req["inputs"]
        sys_in, state_in = inputs.get("system"), inputs.get("state")
        if (sys_in is None) == (state_in is None):
            raise ProviderFailure(
                "input_invalid", "input_error",
                "run tasks take exactly one of the 'system' or 'state' inputs")
        self.from_state = state_in is not None
        src = state_in or sys_in
        meta = src.get("metadata") or {}
        self.units = meta.get("units", "real")
        if self.units not in SUPPORTED_UNITS:
            raise ProviderFailure(
                "input_invalid", "input_error",
                f"LAMMPS unit style '{self.units}' is not supported (supported: "
                f"{', '.join(SUPPORTED_UNITS)}); decks would mis-scale time, "
                "pressure and damping parameters")
        self.atom_style = meta.get("atom_style", "full")
        files = src.get("files") or {}
        if self.from_state:
            self.data_file = self._stage(files, "data", "state.data")
            self.init_file = self._stage(files, "init", "state.in.init", required=False)
            rst = files.get("restart")
            if rst:
                shutil.copy(rst, self.workdir / "state.restart")
        else:
            self.data_file = self._stage(files, "data", "system.data")
            self.init_file = self._stage(files, "init", "system.in.init")
            self.settings_file = self._stage(files, "settings", "system.in.settings", required=False)
            chg = files.get("charges")
            if chg:
                shutil.copy(chg, self.workdir / "system.in.charges")
        if not self.init_file:
            raise ProviderFailure("input_invalid", "input_error",
                                  "input artifact lacks an 'init' include file")

    def _stage(self, files, key, dest, required=True):
        src = files.get(key)
        if not src:
            if required:
                raise ProviderFailure("input_invalid", "input_error",
                                      f"input artifact missing file '{key}'")
            return None
        shutil.copy(src, self.workdir / dest)
        return dest

    # quantity helpers (canonical units: K, bar, fs; unit field honored)
    _TIME_TO_FS = {"fs": 1.0, "ps": 1e3, "ns": 1e6, "us": 1e9, "s": 1e15}
    _PRESS_TO_BAR = {"bar": 1.0, "atm": 1.01325, "kPa": 0.01, "MPa": 10.0,
                     "GPa": 10000.0, "Pa": 1e-5, "psi": 0.0689476}

    def _q(self, name, default=None):
        q = self.params.get(name)
        return q if isinstance(q, dict) else ({"value": q, "unit": None} if q is not None else default)

    @staticmethod
    def _factor(table, unit, name, canonical):
        unit = unit or canonical
        if unit not in table:
            raise ProviderFailure(
                "input_invalid", "input_error",
                f"parameter '{name}': unknown unit '{unit}' "
                f"(accepted: {', '.join(table)})")
        return table[unit]

    def temperature(self, name, default=None):
        q = self._q(name)
        return float(q["value"]) if q else default

    def pressure_bar(self, name="pressure", default=None):
        q = self._q(name)
        if not q:
            return default
        return float(q["value"]) * self._factor(self._PRESS_TO_BAR, q.get("unit"), name, "bar")

    def pressure_lmp(self, name="pressure", default_bar=1.0):
        """Pressure in the deck's unit style: atm (real) or reduced (lj,
        by the 1 bar == P* = 1 input convention)."""
        bar = self.pressure_bar(name, default_bar)
        return bar if self.units == "lj" else bar * ATM_PER_BAR

    def time_fs(self, name, default=None):
        q = self._q(name)
        if not q:
            return default
        return float(q["value"]) * self._factor(self._TIME_TO_FS, q.get("unit"), name, "fs")

    def time_lmp(self, fs):
        """fs -> deck time unit (fs for real, tau for lj)."""
        return fs / FS_PER_TAU if self.units == "lj" else fs

    def timestep_fs(self):
        return self.time_fs("timestep", 1.0)

    def timestep_lammps(self):
        return self.time_lmp(self.timestep_fs())

    def steps(self, duration_fs):
        return max(1, int(round(duration_fs / self.timestep_fs())))

    def stride(self, name, default_fs):
        """Integer step stride for an output interval. The actual interval
        (stride x dt) is what downstream analysis must use; a mismatch with
        the request is reported as a warning, never hidden."""
        want = self.time_fs(name, default_fs)
        dt = self.timestep_fs()
        every = max(1, int(round(want / dt)))
        actual = every * dt
        if not math.isclose(actual, want, rel_tol=1e-9, abs_tol=1e-12):
            self.warnings.append(
                f"{name}: requested {want:g} fs is not a multiple of the "
                f"{dt:g} fs timestep; using every {every} steps = {actual:g} fs")
        return every

    def seed(self):
        s = self.params.get("seed")
        return 12345 if s is None else int(s)


def _preamble(ctx):
    lines = [f"include {ctx.init_file}"]
    if ctx.from_state:
        lines.append(f"read_data {ctx.data_file}")
    else:
        lines.append(f"read_data {ctx.data_file}")
        if getattr(ctx, "settings_file", None):
            lines.append(f"include {ctx.settings_file}")
        if (ctx.workdir / "system.in.charges").is_file():
            lines.append("include system.in.charges")
    lines.append("neighbor 1.0 bin" if ctx.units == "real" else "neighbor 0.3 bin")
    return lines


def _thermo_block(ctx, extra_cols=None, extra_computes=None):
    every = ctx.stride("thermo_interval", 100.0)
    ctx.thermo_every = every
    cols = ["step", "temp", "pe", "ke", "etotal", "press", "vol", "density",
            "enthalpy", "lx", "ly", "lz"]
    lines = []
    if extra_computes:
        lines += extra_computes
        cols += (extra_cols or [])
    lines.append("thermo_style custom " + " ".join(cols))
    # extensive totals in every unit style (lj defaults to per-atom norm)
    lines.append("thermo_modify norm no")
    lines.append(f"thermo {every}")
    return lines


def _dump_block(ctx, name="traj.dcd"):
    every = ctx.stride("sampling_interval", 1000.0)
    ctx.dump_every = every
    return [
        f"dump m3d all dcd {every} {name}",
        "dump_modify m3d unwrap yes",
    ], every


def _finalize(ctx):
    return [
        "write_data final.data",
        "write_restart final.restart",
    ]


def _input_kspace_style(ctx):
    """The kspace_style declared by the staged input decks, or None."""
    style = None
    for name in (ctx.init_file, getattr(ctx, "settings_file", None)):
        if not name:
            continue
        path = ctx.workdir / name
        if not path.is_file():
            continue
        for ln in path.read_text(errors="replace").splitlines():
            tok = ln.split("#", 1)[0].split()
            if len(tok) >= 2 and tok[0] == "kspace_style":
                style = None if tok[1] == "none" else tok[1]
    return style


def _interaction_spec(ctx):
    """Normalized `interaction` parameter, or None.

    kspace: "auto" (default) includes the long-range part whenever the input
    declares a kspace_style — compute group/group defaults to kspace=no,
    which silently drops long-range electrostatics for charged interfaces.
    """
    spec = ctx.params.get("interaction")
    if not spec:
        return None
    ga, gb = spec.get("group_a"), spec.get("group_b")
    if not (ga and gb):
        raise ProviderFailure(
            "input_invalid", "input_error",
            "interaction needs both 'group_a' and 'group_b' LAMMPS group selectors")
    kspace = spec.get("kspace", "auto")
    declared = _input_kspace_style(ctx)
    if kspace in ("auto", None):
        kspace = declared is not None
    elif isinstance(kspace, str):
        kspace = kspace.lower() in ("yes", "true", "on")
    else:
        kspace = bool(kspace)
    if kspace and declared is None:
        raise ProviderFailure(
            "input_invalid", "input_error",
            "interaction.kspace requested but the input defines no kspace_style")
    return {"group_a": ga, "group_b": gb, "kspace": kspace,
            "kspace_style": declared}


def _interaction_block(ctx):
    """Optional group/group interaction energy computes.

    Emits the pair part (and the kspace part when enabled) as separate
    columns, aborts the run when the groups overlap (group/group would count
    intra-group pairs), and prints each group's net charge for provenance.
    """
    spec = _interaction_spec(ctx)
    if not spec:
        return [], []
    lines = [
        f"group m3_ga {spec['group_a']}",
        f"group m3_gb {spec['group_b']}",
        "group m3_gab intersect m3_ga m3_gb",
        "variable m3_nab equal count(m3_gab)",
        f"if \"${{m3_nab}} > 0\" then \"print '{CHECK_MARKER}: interaction "
        f"groups overlap in ${{m3_nab}} atoms'\" \"quit 3\"",
        "variable m3_na equal count(m3_ga)",
        "variable m3_nb equal count(m3_gb)",
        f"if \"${{m3_na}} == 0 || ${{m3_nb}} == 0\" then \"print '{CHECK_MARKER}: "
        f"interaction group is empty (a=${{m3_na}}, b=${{m3_nb}})'\" \"quit 3\"",
    ]
    if ctx.atom_style in ("full", "charge"):
        lines += [
            "variable m3_qa equal charge(m3_ga)",
            "variable m3_qb equal charge(m3_gb)",
            f"print \"{CHARGE_MARKER} ${{m3_qa}} ${{m3_qb}}\"",
        ]
    lines.append("compute m3_eint m3_ga group/group m3_gb pair yes kspace no")
    cols = ["c_m3_eint"]
    if spec["kspace"]:
        lines.append("compute m3_eintk m3_ga group/group m3_gb pair no kspace yes")
        cols.append("c_m3_eintk")
    ctx.interaction = spec
    return lines, cols


# ------------------------------------------------------------------ decks

def _velocity_if_needed(ctx, temperature):
    if ctx.from_state:
        return []  # state.data carries velocities
    if temperature is None:
        raise ProviderFailure(
            "input_invalid", "input_error",
            "velocity initialization needs a temperature when starting from "
            "a SimulationSystem")
    return [f"velocity all create {temperature} {ctx.seed()} mom yes rot yes"]


def deck_minimize(ctx):
    p = ctx.params
    lines = _preamble(ctx)
    relax_fs = ctx.time_fs("relax_duration", 0.0)
    if relax_fs and relax_fs > 0:
        t_relax = ctx.temperature("relax_temperature", 10.0)
        lines += _velocity_if_needed(ctx, t_relax)
        lines += [
            f"fix m3rlx all nve/limit 0.1",
            f"fix m3lan all langevin {t_relax} {t_relax} {ctx.time_lmp(100.0)} {ctx.seed()}",
            f"timestep {ctx.timestep_lammps()}",
            f"run {ctx.steps(relax_fs)}",
            "unfix m3rlx",
            "unfix m3lan",
            "reset_timestep 0",
        ]
    lines += [
        "min_style cg",
        f"minimize {p.get('etol', 1e-6)} {p.get('ftol', 1e-8)} "
        f"{int(p.get('maxiter', 10000))} {int(p.get('maxeval', 100000))}",
    ]
    lines += _finalize(ctx)
    return lines, False


def _pressure_clause(ctx, pdamp_lmp, barostat="nose_hoover"):
    """Barostat keyword clause for fix npt / nph / press/berendsen.

    iso|aniso|tri -> "{style} Pstart Pstop Pdamp" (Pstop = pressure_end or
    Pstart). xyz -> per-axis "x Px0 Px1 Pdamp ..." clauses plus a trailing
    "couple <mode>"; axes without a pressure_<axis> parameter are not
    barostated (LAMMPS semantics). fix press/berendsen has no `tri`.
    """
    p = ctx.params
    style = p.get("pressure_style") or "iso"
    couple = p.get("couple")
    if barostat == "berendsen" and style == "tri":
        raise ProviderFailure(
            "input_invalid", "input_error",
            "pressure_style 'tri' needs the nose_hoover barostat: LAMMPS fix "
            "press/berendsen cannot control triclinic tilt")
    if style == "xyz":
        parts = []
        for axis in ("x", "y", "z"):
            if ctx._q(f"pressure_{axis}") is None:
                continue
            b0 = ctx.pressure_bar(f"pressure_{axis}")
            a0 = ctx.pressure_lmp(f"pressure_{axis}")
            a1 = ctx.pressure_lmp(f"pressure_{axis}_end", b0)
            parts.append(f"{axis} {a0} {a1} {pdamp_lmp}")
        if not parts:
            raise ProviderFailure(
                "input_invalid", "input_error",
                "pressure_style 'xyz' requires at least one of "
                "pressure_x/pressure_y/pressure_z")
        return " ".join(parts) + f" couple {couple or 'xyz'}"
    if couple and couple != "xyz":
        raise ProviderFailure(
            "input_invalid", "input_error",
            "couple is only valid with pressure_style 'xyz'")
    if any(ctx._q(f"pressure_{a}") is not None for a in "xyz"):
        raise ProviderFailure(
            "input_invalid", "input_error",
            "pressure_x/y/z are only valid with pressure_style 'xyz'")
    b0 = ctx.pressure_bar("pressure", 1.0)
    p0 = ctx.pressure_lmp("pressure")
    p1 = ctx.pressure_lmp("pressure_end", b0)
    return f"{style} {p0} {p1} {pdamp_lmp}"


def _ensemble_fixes(ctx, ensemble):
    """[(fix_id, fix_command)] for an MD run.

    Every supported thermostat/barostat pair maps to an explicit fix set;
    the deck's unfix lines are derived from this list, never re-guessed.

      thermostat   barostat     fixes
      nose_hoover  nose_hoover  npt
      langevin     nose_hoover  nph + langevin
      berendsen    nose_hoover  nph + temp/berendsen
      nose_hoover  berendsen    nvt + press/berendsen
      langevin     berendsen    nve + langevin + press/berendsen
      berendsen    berendsen    nve + temp/berendsen + press/berendsen
    """
    if ensemble == "nve":
        return [("m3", "fix m3 all nve")]
    p = ctx.params
    t0 = ctx.temperature("temperature")
    if t0 is None:
        raise ProviderFailure("input_invalid", "input_error",
                              f"{ensemble} run requires parameter 'temperature'")
    t1 = ctx.temperature("temperature_end") or t0
    tdamp = ctx.time_lmp(ctx.time_fs("tdamp", 100.0))
    tstat = p.get("thermostat") or "nose_hoover"
    if tstat not in THERMOSTATS:
        raise ProviderFailure("input_invalid", "input_error",
                              f"unknown thermostat '{tstat}' (use {', '.join(THERMOSTATS)})")

    def thermostat_only():  # velocity-rescaling fix on top of an integrator
        if tstat == "langevin":
            return ("m3t", f"fix m3t all langevin {t0} {t1} {tdamp} {ctx.seed()}")
        return ("m3t", f"fix m3t all temp/berendsen {t0} {t1} {tdamp}")

    if ensemble == "nvt":
        if tstat == "nose_hoover":
            return [("m3", f"fix m3 all nvt temp {t0} {t1} {tdamp}")]
        return [("m3", "fix m3 all nve"), thermostat_only()]

    if ensemble != "npt":
        raise ProviderFailure("input_invalid", "input_error",
                              f"unknown ensemble '{ensemble}'")
    bstat = p.get("barostat") or "nose_hoover"
    if bstat not in BAROSTATS:
        raise ProviderFailure("input_invalid", "input_error",
                              f"unknown barostat '{bstat}' (use {', '.join(BAROSTATS)})")
    pdamp = ctx.time_lmp(ctx.time_fs("pdamp", 1000.0))
    pclause = _pressure_clause(ctx, pdamp, barostat=bstat)
    if bstat == "nose_hoover":
        if tstat == "nose_hoover":
            return [("m3", f"fix m3 all npt temp {t0} {t1} {tdamp} {pclause}")]
        return [("m3", f"fix m3 all nph {pclause}"), thermostat_only()]
    if tstat == "nose_hoover":
        fixes = [("m3", f"fix m3 all nvt temp {t0} {t1} {tdamp}")]
    else:
        fixes = [("m3", "fix m3 all nve"), thermostat_only()]
    fixes.append(("m3p", f"fix m3p all press/berendsen {pclause}{_berendsen_modulus(ctx)}"))
    return fixes


# press/berendsen rescales by dt/Pdamp * dP/modulus per step; LAMMPS's
# default modulus (10 pressure units) suits lj but makes atm-unit systems
# blow up (a 250 atm imbalance rescales the volume by ~5% per step).
REAL_BERENDSEN_MODULUS_BAR = 20000.0  # ~2 GPa, liquids / polymer melts


def _berendsen_modulus(ctx):
    """` modulus <K>` clause: the berendsen_modulus parameter, else ~2 GPa
    for real units (overestimating K only slows the barostat; an
    underestimate destabilizes it), else the LAMMPS default for lj."""
    bar = ctx.pressure_bar("berendsen_modulus")
    if bar is None and ctx.units == "real":
        bar = REAL_BERENDSEN_MODULUS_BAR
    if bar is None:
        return ""
    if bar <= 0:
        raise ProviderFailure("input_invalid", "input_error",
                              "berendsen_modulus must be positive")
    return f" modulus {bar if ctx.units == 'lj' else bar * ATM_PER_BAR}"


def deck_run(ctx, ensemble):
    lines = _preamble(ctx)
    lines += _velocity_if_needed(ctx, ctx.temperature("temperature"))
    fixes = _ensemble_fixes(ctx, ensemble)
    lines += [cmd for _, cmd in fixes]
    created = [fid for fid, _ in fixes]
    if ensemble in ("nvt", "npt"):
        # remove net linear + angular momentum drift during equilibration
        lines.append("fix m3mom all momentum 1000 linear 1 1 1 angular")
        created.append("m3mom")

    extra_computes, extra_cols = _interaction_block(ctx)
    lines += _thermo_block(ctx, extra_cols=extra_cols, extra_computes=extra_computes)
    dump_lines, _ = _dump_block(ctx)
    lines += dump_lines
    lines.append(f"timestep {ctx.timestep_lammps()}")
    duration = ctx.time_fs("duration")
    if not duration:
        raise ProviderFailure("input_invalid", "input_error",
                              "run task requires parameter 'duration'")
    ctx.n_steps = ctx.steps(duration)
    lines.append(f"run {ctx.n_steps}")
    lines += [f"unfix {fid}" for fid in reversed(created)]
    lines += _finalize(ctx)
    return lines, True


def deck_soft_pushoff(ctx):
    lines = _preamble(ctx)
    t = ctx.temperature("temperature", 300.0)
    duration = ctx.time_fs("duration")
    if not duration:
        raise ProviderFailure("input_invalid", "input_error",
                              "run_soft_pushoff requires 'duration'")
    steps = ctx.steps(duration)
    ctx.n_steps = steps
    lines += _velocity_if_needed(ctx, t)
    lines += [
        "variable m3pref equal ramp(0.0,1.0)",
        # soften pair interactions, ramping to full over the run
        "fix m3soft all adapt 0 pair lj/cut epsilon * * v_m3pref scale yes",
        "fix m3 all nve/limit 0.05",
        f"fix m3t all langevin {t} {t} {ctx.time_lmp(100.0)} {ctx.seed()}",
    ]
    lines += _thermo_block(ctx)
    lines.append(f"timestep {ctx.timestep_lammps()}")
    lines.append(f"run {steps}")
    lines += ["unfix m3t", "unfix m3", "unfix m3soft"]
    lines += _finalize(ctx)
    return lines, False


def deck_deform(ctx):
    """Uniaxial constant-engineering-strain-rate deformation.

    Strain is measured from the box (L - L0)/L0, so it is exact whatever the
    unit style; atoms are remapped with the box (`remap x`, the default for
    solid deformation — `remap v` is for SLLOD flows and would make the
    thermostat fight the imposed velocity profile).
    """
    p = ctx.params
    lines = _preamble(ctx)
    t = ctx.temperature("temperature")
    if t is None:
        raise ProviderFailure("input_invalid", "input_error",
                              "run_deform requires parameter 'temperature'")
    direction = p.get("direction") or "z"
    if direction not in ("x", "y", "z"):
        raise ProviderFailure("input_invalid", "input_error",
                              f"direction must be x, y or z (got {direction!r})")
    erate = float(p["strain_rate"]) if p.get("strain_rate") is not None else 1e-7  # 1/fs
    max_strain = float(p["max_strain"]) if p.get("max_strain") is not None else 0.5
    if erate <= 0 or max_strain <= 0:
        raise ProviderFailure("input_invalid", "input_error",
                              "strain_rate and max_strain must be positive")
    dt = ctx.timestep_fs()
    steps = max(1, int(round(max_strain / (erate * dt))))
    ctx.n_steps = steps
    tdamp = ctx.time_lmp(ctx.time_fs("tdamp", 100.0))
    erate_lmp = erate * FS_PER_TAU if ctx.units == "lj" else erate  # 1/time unit

    lines += _velocity_if_needed(ctx, t)
    every = ctx.stride("sampling_interval", 500.0)
    ctx.dump_every = every
    lines += [
        f"variable m3tmp equal l{direction}",
        "variable m3L0 equal ${m3tmp}",
        f"fix m3def all deform 1 {direction} erate {erate_lmp} remap x units box",
        f"fix m3 all nvt temp {t} {t} {tdamp}",
        f"variable m3strain equal (l{direction}-v_m3L0)/v_m3L0",
        f"variable m3stress equal -p{direction}{direction}",
        f"fix m3out all print {every} \"${{m3strain}} ${{m3stress}}\" "
        f"file stress_strain.csv screen no",
        f"timestep {ctx.timestep_lammps()}",
        f"run {steps}",
        "unfix m3out",
        "unfix m3",
        "unfix m3def",
    ]
    lines += _finalize(ctx)
    return lines, False


# ------------------------------------------------------------------ execution

def _pk_args(opts):
    """Flatten a package-options dict into `key value` tokens (bool -> on/off)."""
    args = []
    for k, v in (opts or {}).items():
        if isinstance(v, bool):
            v = "on" if v else "off"
        args += [str(k), str(v)]
    return args


def _gpu_flags(req, exe, ng):
    """Acceleration flags for LAMMPS's two GPU frameworks (docs: Speed_gpu,
    Speed_kokkos), chosen against the binary's installed packages.

    config["gpu"] is bool | dict:
      backend: auto | kokkos | gpu   (auto: probe, KOKKOS preferred)
      options: extra -pk args, e.g. {split: -1, neigh: no} for the GPU
               package or {comm: device} for Kokkos
    Returns (flags, mode_label). Raises gpu_backend_unavailable when the
    requested backend is not compiled into `exe`.
    """
    cfg = req.get("config") or {}
    gcfg = cfg.get("gpu")
    gcfg = {} if gcfg is True else (gcfg or {})
    have = _installed_packages(exe)
    backend = gcfg.get("backend") or "auto"
    if backend == "auto":
        backend = "kokkos" if "KOKKOS" in have else \
                  "gpu" if "GPU" in have else None
    if backend not in ("kokkos", "gpu"):
        raise ProviderFailure(
            "gpu_backend_unavailable", "environment_error",
            f"GPU requested ({ng} device(s)) but {exe} has neither KOKKOS nor "
            "GPU package installed",
            recoverable=False)
    pkg = "KOKKOS" if backend == "kokkos" else "GPU"
    if pkg not in have:
        raise ProviderFailure(
            "gpu_backend_unavailable", "environment_error",
            f"gpu.backend='{backend}' requested but {pkg} is not among "
            f"{exe}'s installed packages",
            recoverable=False)
    pk = _pk_args(gcfg.get("options"))
    if backend == "kokkos":
        flags = ["-k", "on", "g", str(ng), "-sf", "kk"]
        if pk:
            flags += ["-pk", "kokkos"] + pk
        return flags, f"kokkos-gpu x{ng}"
    return ["-sf", "gpu", "-pk", "gpu", str(ng)] + pk, f"gpu-pkg x{ng}"


def _parallel_cmd(req, exe):
    """Resolve launch command + env from declared resources / engine config.

    Precedence: req["resources"]["cpu"] (per-task/step) > config["np"] > 1.
    config["mpi"] == false selects OpenMP threading instead of MPI ranks.

    GPU: enabled when req["resources"]["gpu"] > 0 (scheduled via Slurm
    --gres) or config["gpu"] is set (true, or a dict — see _gpu_flags).
    MPI ranks default to ranks_per_gpu x N_gpu (1 x N_gpu unless config
    overrides); host threads per rank = resources.cpu / ranks. With the
    Kokkos backend, config["gpu"]["devices"] pins CUDA_VISIBLE_DEVICES on
    non-Slurm hosts. Styles without a /kk or /gpu variant silently fall
    back to the host version (LAMMPS suffix semantics).
    """
    resources = req.get("resources") or {}
    cfg = req.get("config") or {}
    try:
        ncpu = int(resources.get("cpu") or cfg.get("np") or 1)
    except (TypeError, ValueError):
        ncpu = 1
    ncpu = max(1, ncpu)
    base = [exe, "-in", "in.m3flow", "-log", "log.lammps", "-screen", "none"]

    gcfg = cfg.get("gpu")
    gcfg = {} if gcfg is True else (gcfg or {})
    try:
        ng = int(resources.get("gpu") or 0)
    except (TypeError, ValueError):
        ng = 0
    devices = gcfg.get("devices")
    if isinstance(devices, int):
        devices = [devices]
    if ng == 0 and (cfg.get("gpu") is True or gcfg):
        ng = len(devices) if devices else 1
    if ng > 0:
        gpu_flags, gpu_mode = _gpu_flags(req, exe, ng)
        try:
            np_ = int(cfg.get("np") or 0)
        except (TypeError, ValueError):
            np_ = 0
        if np_ <= 0:
            try:
                np_ = max(1, int(gcfg.get("ranks_per_gpu") or 1)) * ng
            except (TypeError, ValueError):
                np_ = ng
        threads = max(1, ncpu // np_)
        env = dict(os.environ, OMP_NUM_THREADS=str(threads))
        if devices and "CUDA_VISIBLE_DEVICES" not in os.environ:
            env["CUDA_VISIBLE_DEVICES"] = ",".join(str(d) for d in devices)
        if np_ == 1:
            return base + gpu_flags, env, gpu_mode
        launcher = shutil.which(cfg.get("launcher") or "mpirun")
        if not launcher:
            return base + gpu_flags, env, f"{gpu_mode} (mpirun not found)"
        return [launcher, "-np", str(np_)] + base + gpu_flags, env, \
            f"mpi x{np_} + {gpu_mode}"

    if ncpu == 1:
        return base, None, "serial"
    if cfg.get("mpi") is False:
        env = dict(os.environ, OMP_NUM_THREADS=str(ncpu))
        return base + ["-sf", "omp"], env, f"openmp x{ncpu}"
    launcher = shutil.which(cfg.get("launcher") or "mpirun")
    if not launcher:
        return base, None, f"serial (mpirun not found, requested np={ncpu})"
    return [launcher, "-np", str(ncpu)] + base, None, f"mpi x{ncpu}"


def _run_lammps(ctx, deck_lines, has_trajectory):
    deck_path = ctx.workdir / "in.m3flow"
    deck_path.write_text("\n".join(deck_lines) + "\n")
    cmd, env, mode = _parallel_cmd(ctx.req, _binary(ctx.req))
    with open(ctx.workdir / "stdout.log", "w") as out:
        out.write(f"m3flow: launching LAMMPS ({mode}): {' '.join(cmd)}\n")
        out.flush()
        proc = subprocess.run(cmd, cwd=ctx.workdir, stdout=out, stderr=out,
                              env=env, timeout=24 * 3600)
    log_path = ctx.workdir / "log.lammps"
    log_text = log_path.read_text(errors="replace") if log_path.is_file() else ""
    # stderr (merged into stdout.log) carries errors that never reach
    # log.lammps — e.g. Kokkos CUDA init failures abort before the log opens
    stdout_path = ctx.workdir / "stdout.log"
    stderr_tail = ""
    if stdout_path.is_file():
        stderr_tail = "\n".join(
            stdout_path.read_text(errors="replace").splitlines()[-40:])
    _classify_failure(proc.returncode, log_text, stderr_tail)
    return log_text


def _classify_failure(returncode, log_text, stderr_tail=""):
    tail = "\n".join(log_text.splitlines()[-40:])
    combined = log_text + "\n" + stderr_tail
    m = re.search(rf"^{CHECK_MARKER}:\s*(.+)$", log_text, re.M)
    if m:
        raise ProviderFailure("input_invalid", "input_error",
                              f"pre-run check failed: {m.group(1).strip()}",
                              recoverable=False, raw_log=tail)
    if "Lost atoms" in log_text:
        raise ProviderFailure("lost_atoms", "execution_error",
                              "LAMMPS lost atoms during the run",
                              recoverable=False, raw_log=tail)
    if re.search(r"\bnan\b|\bNaN\b", log_text) and "Total wall time" not in log_text:
        raise ProviderFailure("nan_detected", "execution_error",
                              "NaN appeared in the simulation",
                              recoverable=False, raw_log=tail)
    if "Energy too large" in log_text or "Bond atoms %" in log_text:
        raise ProviderFailure("energy_blowup", "execution_error",
                              "energy blowup / topology corruption",
                              recoverable=False, raw_log=tail)
    if re.search(r"without (GPU|KOKKOS) package installed|"
                 r"cudaError|CUDA driver|cuInit|Could not (find|open).*GPU|"
                 r"no GPU(s)? (found|present|detected)", combined, re.I):
        raise ProviderFailure(
            "gpu_backend_unavailable", "environment_error",
            "requested GPU backend unavailable (package not compiled in, or "
            "no usable GPU/driver on this host)",
            recoverable=False, raw_log=tail or stderr_tail)
    if "Total wall time" not in log_text:
        m = re.search(r"ERROR:?\s*(.+)", tail)
        msg = m.group(1) if m else f"exit code {returncode}, no completion marker"
        raise ProviderFailure(
            "simulation_incomplete", "execution_error",
            f"simulation did not complete: {msg}",
            recoverable=False, raw_log=tail)


def _parse_thermo(log_text):
    """Extract the last thermo block as {columns, rows}."""
    lines = log_text.splitlines()
    blocks = []
    header_idx = None
    for i, ln in enumerate(lines):
        s = ln.lstrip()
        if s.startswith("Step "):
            header_idx = i
        elif header_idx is not None and s.startswith(("Loop time", "ERROR", "Minimization", "Total wall")):
            blocks.append((header_idx, i))
            header_idx = None
    if not blocks:
        return None
    h, end = blocks[-1]
    cols = lines[h].split()
    rows = []
    for ln in lines[h + 1:end]:
        parts = ln.split()
        if len(parts) != len(cols):
            continue
        try:
            rows.append([float(x) for x in parts])
        except ValueError:
            break
    return {"columns": cols, "rows": rows} if rows else None


def _parse_group_charges(log_text):
    m = re.search(rf"^{CHARGE_MARKER}\s+(\S+)\s+(\S+)", log_text, re.M)
    if not m:
        return None
    try:
        return [float(m.group(1)), float(m.group(2))]
    except ValueError:
        return None


def _dcd_frames(path):
    try:
        with open(path, "rb") as f:
            head = f.read(96)
        if len(head) < 96 or head[:4] != b"\x54\x00\x00\x00":
            return 0
        return struct.unpack("<i", head[8:12])[0]
    except Exception:
        return 0


# thermo keyword -> (quantity, real-units suffix); lj columns get "_lj"
_THERMO_COLUMNS = {
    "Temp": ("temp", "K"), "Press": ("press", "atm"), "Volume": ("vol", "A3"),
    "Density": ("density", "g_cm3"), "PotEng": ("pe", "kcal_mol"),
    "KinEng": ("ke", "kcal_mol"), "TotEng": ("etotal", "kcal_mol"),
    "Enthalpy": ("enthalpy", "kcal_mol"), "Lx": ("lx", "A"),
    "Ly": ("ly", "A"), "Lz": ("lz", "A"),
    "c_m3_eint": ("e_interaction_pair", "kcal_mol"),
    "c_m3_eintk": ("e_interaction_kspace", "kcal_mol"),
}


def _thermo_column_name(ctx, keyword):
    quantity, real_unit = _THERMO_COLUMNS[keyword]
    return f"{quantity}_{'lj' if ctx.units == 'lj' else real_unit}"


def _write_thermo_csv(ctx, thermo, dest="thermo.csv"):
    """Thermo block -> CSV with unit-tagged columns.

    real: time_fs + <q>_<physical unit>; lj: time_tau + <q>_lj (reduced).
    With an interaction compute, e_interaction_<unit> is the total
    (pair + kspace when computed) next to its components.
    """
    dt_fs = ctx.timestep_fs()
    lj = ctx.units == "lj"
    time_col, time_scale = ("time_tau", dt_fs / FS_PER_TAU) if lj else ("time_fs", dt_fs)
    cols = thermo["columns"]
    keep = [(i, _thermo_column_name(ctx, c)) for i, c in enumerate(cols)
            if c in _THERMO_COLUMNS]
    step_i = cols.index("Step") if "Step" in cols else None
    pair_i = cols.index("c_m3_eint") if "c_m3_eint" in cols else None
    kspace_i = cols.index("c_m3_eintk") if "c_m3_eintk" in cols else None
    out_cols = ([time_col] if step_i is not None else []) + [n for _, n in keep]
    if pair_i is not None:
        out_cols.append(f"e_interaction_{'lj' if lj else 'kcal_mol'}")
    lines = [",".join(out_cols)]
    for row in thermo["rows"]:
        vals = []
        if step_i is not None:
            vals.append(f"{row[step_i] * time_scale:.10g}")
        vals += [f"{row[i]:.10g}" for i, _ in keep]
        if pair_i is not None:
            total = row[pair_i] + (row[kspace_i] if kspace_i is not None else 0.0)
            vals.append(f"{total:.10g}")
        lines.append(",".join(vals))
    Path(ctx.workdir / dest).write_text("\n".join(lines) + "\n")
    return dest, len(thermo["rows"]), out_cols


# ------------------------------------------------------------------ outputs + validation

def _common_validation(ctx, log_text, has_trajectory, traj_name="traj.dcd"):
    completed = "Total wall time" in log_text
    nan = re.search(r"\bnan\b", log_text, re.I) is not None
    lost = "Lost atoms" in log_text
    out = [
        verdict("simulation_completed", completed,
                None if completed else "no 'Total wall time' marker in log"),
        verdict("no_nan", not nan),
        verdict("no_lost_atoms", not lost,
                None if not lost else "LAMMPS reported lost atoms"),
    ]
    if has_trajectory:
        n = _dcd_frames(ctx.workdir / traj_name)
        out.append(verdict("trajectory_readable", n > 0,
                           f"{n} frames in {traj_name}"))
    return out


def _unit_manifest(ctx):
    """How to read every number this run emits."""
    if ctx.units == "lj":
        return {"units": "lj", "time_unit": "tau",
                "reduced_input_convention": {"fs_per_tau": FS_PER_TAU,
                                             "temperature": "1 K == T* = 1",
                                             "pressure": "1 bar == P* = 1"}}
    return {"units": "real", "time_unit": "fs"}


def _state_artifact(ctx, meta_extra=None):
    meta = dict(ctx.req["inputs"].get("state") or ctx.req["inputs"]["system"]).get("metadata") or {}
    meta = {**meta, **(meta_extra or {})}
    files = {"data": "final.data", "restart": "final.restart", "init": ctx.init_file}
    return artifact("SimulationState", files=files, metadata=meta)


def _run_record(ctx):
    """Actual (not requested) step counts and durations of the run."""
    dt = ctx.timestep_fs()
    rec = {"timestep_fs": dt}
    if ctx.n_steps is not None:
        rec["n_steps"] = ctx.n_steps
        rec["duration_fs"] = ctx.n_steps * dt
    return rec


def _thermo_artifact(ctx, thermo, ensemble):
    csv, n_rows, out_cols = _write_thermo_csv(ctx, thermo)
    dt = ctx.timestep_fs()
    meta = {**_unit_manifest(ctx), **_run_record(ctx),
            "ensemble": ensemble,
            "thermo_norm": False,
            "temperature_K": ctx.temperature("temperature"),
            "pressure_bar": ctx.pressure_bar("pressure")}
    if ctx.thermo_every:
        meta["thermo_stride"] = ctx.thermo_every
        meta["thermo_interval_fs"] = ctx.thermo_every * dt
    if ensemble in ("nvt", "npt"):
        meta["thermostat"] = ctx.params.get("thermostat") or "nose_hoover"
    if ensemble == "npt":
        meta["barostat"] = ctx.params.get("barostat") or "nose_hoover"
    interaction = getattr(ctx, "interaction", None)
    if interaction:
        meta["interaction"] = {
            "group_a": interaction["group_a"], "group_b": interaction["group_b"],
            "kspace_included": interaction["kspace"],
            "kspace_style": interaction["kspace_style"],
            "group_charges": getattr(ctx, "group_charges", None),
            "observable": "group/group interaction energy (pair"
                          + (" + kspace" if interaction["kspace"] else " only") + ")",
        }
    return artifact("ThermodynamicSeries", files={"csv": csv}, metadata=meta,
                    data={"columns": out_cols, "lammps_columns": thermo["columns"],
                          "n_rows": n_rows})


def _run_outputs(ctx, has_trajectory, thermo, request_meta_extra=None):
    ensemble = (request_meta_extra or {}).get("ensemble")
    outputs = {"state": _state_artifact(ctx, request_meta_extra)}
    if has_trajectory:
        n_frames = _dcd_frames(ctx.workdir / "traj.dcd")
        dt = ctx.timestep_fs()
        stride = ctx.dump_every or 1
        meta = {
            **_unit_manifest(ctx), **_run_record(ctx),
            "format": "dcd",
            "topology_format": "lammps_data",
            "coordinates": "unwrapped",
            "dump_stride": stride,
            "frame_interval_fs": stride * dt,
        }
        if ctx.units == "lj":
            meta["frame_interval_tau"] = stride * dt / FS_PER_TAU
        outputs["trajectory"] = artifact(
            "Trajectory",
            files={"dcd": "traj.dcd", "topology": ctx.data_file},
            metadata=meta,
            data={"n_frames": n_frames})
    outputs["log"] = artifact("SimulationLog", files={"log": "log.lammps"})
    if thermo:
        outputs["thermo"] = _thermo_artifact(ctx, thermo, ensemble)
    return outputs


def _finish(ctx, result):
    if ctx.warnings:
        result["warnings"] = list(ctx.warnings)
    return result


# ------------------------------------------------------------------ task handlers

def _run_task(req, ensemble):
    ctx = Ctx(req)
    if ensemble == "minimize":
        deck, has_traj = deck_minimize(ctx)
        extra = {"ensemble": "minimize"}
    elif ensemble == "pushoff":
        deck, has_traj = deck_soft_pushoff(ctx)
        extra = {"ensemble": "soft_pushoff"}
    else:
        deck, has_traj = deck_run(ctx, ensemble)
        extra = {"ensemble": ensemble}
    log_text = _run_lammps(ctx, deck, has_traj)
    ctx.group_charges = _parse_group_charges(log_text)
    thermo = None if ensemble in ("minimize",) else _parse_thermo(log_text)
    outputs = _run_outputs(ctx, has_traj, thermo, extra)
    validation = _common_validation(ctx, log_text, has_traj)
    return _finish(ctx, {"outputs": outputs, "validation": validation})


def energy_minimize(req):
    ctx = Ctx(req)
    deck, _ = deck_minimize(ctx)
    log_text = _run_lammps(ctx, deck, False)
    outputs = {
        "state": _state_artifact(ctx, {"ensemble": "minimize"}),
        "log": artifact("SimulationLog", files={"log": "log.lammps"}),
    }
    return _finish(ctx, {"outputs": outputs,
                         "validation": _common_validation(ctx, log_text, False)})


def run_nvt(req):
    return _run_task(req, "nvt")


def run_npt(req):
    return _run_task(req, "npt")


def run_nve(req):
    return _run_task(req, "nve")


def run_soft_pushoff(req):
    ctx = Ctx(req)
    deck, _ = deck_soft_pushoff(ctx)
    log_text = _run_lammps(ctx, deck, False)
    thermo = _parse_thermo(log_text)
    outputs = {
        "state": _state_artifact(ctx, {"ensemble": "soft_pushoff"}),
        "log": artifact("SimulationLog", files={"log": "log.lammps"}),
    }
    if thermo:
        outputs["thermo"] = _thermo_artifact(ctx, thermo, "soft_pushoff")
    return _finish(ctx, {"outputs": outputs,
                         "validation": _common_validation(ctx, log_text, False)})


def run_deform(req):
    ctx = Ctx(req)
    deck, _ = deck_deform(ctx)
    log_text = _run_lammps(ctx, deck, False)
    # stress_strain.csv written by fix print: "strain stress" per line, the
    # stress in the deck's pressure unit (atm for real, reduced for lj)
    to_gpa = GPA_PER_LAMMPS_PRESSURE.get(ctx.units)
    stress_col, stress_unit = ("stress_GPa", "GPa") if to_gpa else ("stress_lj", "lj")
    series_path = ctx.workdir / "stress_strain.csv"
    strain, stress = [], []
    if series_path.is_file():
        for ln in series_path.read_text().splitlines():
            parts = ln.split()
            if len(parts) >= 2 and not ln.startswith("#"):
                try:
                    s, sig = float(parts[0]), float(parts[1])
                except ValueError:
                    continue
                strain.append(s)
                stress.append(sig * to_gpa if to_gpa else sig)
    dt = ctx.timestep_fs()
    outputs = {
        "state": _state_artifact(ctx, {"ensemble": "deform"}),
        "series": artifact(
            "StressStrainSeries",
            files={"csv": "stress_strain_series.csv"},
            metadata={**_unit_manifest(ctx), **_run_record(ctx),
                      "direction": ctx.params.get("direction") or "z",
                      "stress_unit": stress_unit,
                      "stress_column": stress_col,
                      "strain_definition": "engineering, (L - L0)/L0 from the box",
                      "sample_stride": ctx.dump_every,
                      "sample_interval_fs": (ctx.dump_every or 1) * dt},
            data={"n_points": len(strain)}),
        "log": artifact("SimulationLog", files={"log": "log.lammps"}),
    }
    # normalize into a real csv with headers
    with open(ctx.workdir / "stress_strain_series.csv", "w") as f:
        f.write(f"strain,{stress_col}\n")
        for s, g in zip(strain, stress):
            f.write(f"{s:.8g},{g:.8g}\n")
    return _finish(ctx, {"outputs": outputs,
                         "validation": _common_validation(ctx, log_text, False)})


# ------------------------------------------------------------------ plumbing

def cli():
    provider = Provider(
        name="lammps",
        version=PROVIDER_VERSION,
        engine=_engine,
        tasks={
            "energy_minimize": energy_minimize,
            "run_nvt": run_nvt,
            "run_npt": run_npt,
            "run_nve": run_nve,
            "run_soft_pushoff": run_soft_pushoff,
            "run_deform": run_deform,
        },
        # MPI rank count / launcher only change the domain decomposition;
        # GPU backends run different kernels and stay in the cache key.
        scheduling_config_keys=("np", "mpi", "launcher"))
    raise SystemExit(provider.cli())


if __name__ == "__main__":
    cli()
