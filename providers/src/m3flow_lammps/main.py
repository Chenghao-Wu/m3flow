"""m3flow-lammps: MD tasks via LAMMPS (deck generation, log parsing, validation).

Implements: energy_minimize, run_nvt, run_npt, run_nve, run_soft_pushoff,
run_deform (tasks/lammps/*.yaml).

Conventions:
  - canonical quantities arrive as {value, unit} in canonical units
    (K, bar, fs); LAMMPS real units need atm for pressure (x0.986923)
  - `units lj` systems (CG) map 1 tau = 1000 fs, so the duration/timestep
    *ratio* (step count) is exact in both unit systems
  - state chaining: every run ends with write_data (coeffs + velocities) and
    write_restart; the next deck re-reads them
"""

from __future__ import annotations

import json
import math
import os
import re
import shutil
import struct
import subprocess
from pathlib import Path

from m3flow_provider import (Provider, ProviderFailure, artifact, verdict)

PROVIDER_VERSION = "0.3.1"
ATM_PER_BAR = 0.986923
FS_PER_TAU = 1000.0  # lj-time convention for CG systems


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


def _engine():
    exe = _binary()
    out = subprocess.run([exe, "-h"], capture_output=True, text=True, timeout=60)
    text = out.stdout + out.stderr
    for line in text.splitlines():
        if "Large-scale Atomic" in line:
            # "... Simulator - 22 Jul 2025 - Update 3"
            version = line.split("Simulator", 1)[-1].strip(" -")
            return {"name": "lammps", "version": version or line.strip()[:60]}
    return {"name": "lammps", "version": "unknown"}


# ------------------------------------------------------------------ context

class Ctx:
    def __init__(self, req):
        self.req = req
        self.params = req["parameters"]
        self.workdir = Path(req["workdir"])
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
    _TIME_TO_FS = {"fs": 1.0, "ps": 1e3, "ns": 1e6, "s": 1e15}
    _PRESS_TO_BAR = {"bar": 1.0, "atm": 1.01325, "kPa": 0.01, "MPa": 10.0,
                     "GPa": 10000.0, "Pa": 1e-5}

    def _q(self, name, default=None):
        q = self.params.get(name)
        return q if isinstance(q, dict) else ({"value": q, "unit": None} if q is not None else default)

    def temperature(self, name, default=None):
        q = self._q(name)
        return float(q["value"]) if q else default

    def pressure_atm(self, name="pressure", default_bar=1.0):
        q = self._q(name)
        bar = float(q["value"]) * self._PRESS_TO_BAR.get(q.get("unit") or "bar", 1.0) if q else default_bar
        return bar * ATM_PER_BAR

    def time_fs(self, name, default=None):
        q = self._q(name)
        if not q:
            return default
        return float(q["value"]) * self._TIME_TO_FS.get(q.get("unit") or "fs", 1.0)

    def timestep_lammps(self):
        dt = self.time_fs("timestep", 1.0)
        return dt / FS_PER_TAU if self.units == "lj" else dt

    def steps(self, duration_fs):
        dt = self.time_fs("timestep", 1.0)
        return max(1, int(round(duration_fs / dt)))


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
    p = ctx.params
    every_fs = ctx.time_fs("thermo_interval", 100.0)
    every = max(1, int(round(every_fs / ctx.time_fs("timestep", 1.0))))
    cols = ["step", "temp", "pe", "ke", "etotal", "press", "vol", "density", "enthalpy"]
    lines = []
    if extra_computes:
        lines += extra_computes
        cols += (extra_cols or [])
    lines.append("thermo_style custom " + " ".join(cols))
    lines.append(f"thermo {every}")
    return lines


def _dump_block(ctx, name="traj.dcd"):
    p = ctx.params
    every_fs = ctx.time_fs("sampling_interval", 1000.0)
    every = max(1, int(round(every_fs / ctx.time_fs("timestep", 1.0))))
    return [
        f"dump m3d all dcd {every} {name}",
        "dump_modify m3d unwrap yes",
    ], every


def _finalize(ctx):
    return [
        "write_data final.data",
        "write_restart final.restart",
    ]


def _interaction_block(ctx):
    """Optional group/group interaction energy compute."""
    spec = ctx.params.get("interaction")
    if not spec:
        return [], []
    ga, gb = spec.get("group_a"), spec.get("group_b")
    if not (ga and gb):
        return [], []
    lines = [
        f"group m3_ga {ga}",
        f"group m3_gb {gb}",
        # compute ID g1 group/group g2 (pairwise only; kspace excluded,
        # the standard convention for interfacial E_int)
        "compute m3_eint m3_ga group/group m3_gb",
    ]
    return lines, ["c_m3_eint"]


# ------------------------------------------------------------------ decks

def _velocity_if_needed(ctx, temperature):
    if ctx.from_state:
        return []  # state.data carries velocities
    if temperature is None:
        raise ProviderFailure(
            "input_invalid", "input_error",
            "velocity initialization needs a temperature when starting from "
            "a SimulationSystem")
    seed = int(ctx.params.get("seed") or 12345)
    return [f"velocity all create {temperature} {seed} mom yes rot yes"]


def deck_minimize(ctx):
    p = ctx.params
    lines = _preamble(ctx)
    relax_fs = ctx.time_fs("relax_duration", 0.0)
    if relax_fs and relax_fs > 0:
        t_relax = ctx.temperature("relax_temperature", 10.0)
        seed = int(p.get("seed") or 12345)
        lines += _velocity_if_needed(ctx, t_relax)
        lines += [
            f"fix m3rlx all nve/limit 0.1",
            f"fix m3lan all langevin {t_relax} {t_relax} 100.0 {seed}",
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


def deck_run(ctx, ensemble):
    p = ctx.params
    lines = _preamble(ctx)
    t0 = ctx.temperature("temperature")
    t1 = ctx.temperature("temperature_end") or t0
    tdamp = ctx.time_fs("tdamp", 100.0)
    pdamp = ctx.time_fs("pdamp", 1000.0)
    if ctx.units == "lj":
        tdamp_lmp, pdamp_lmp = tdamp / FS_PER_TAU, pdamp / FS_PER_TAU
    else:
        tdamp_lmp, pdamp_lmp = tdamp, pdamp
    seed = int(p.get("seed") or 12345)

    if ensemble == "nve":
        lines += _velocity_if_needed(ctx, t0)
        lines.append("fix m3 all nve")
    else:
        lines += _velocity_if_needed(ctx, t0)
        tstat = p.get("thermostat") or "nose_hoover"
        if ensemble == "nvt":
            if tstat == "langevin":
                lines += [f"fix m3 all nve",
                          f"fix m3t all langevin {t0} {t1} {tdamp_lmp} {seed}"]
            elif tstat == "berendsen":
                lines += [f"fix m3 all nve",
                          f"fix m3t all temp/berendsen {t0} {t1} {tdamp_lmp}"]
            else:
                lines.append(f"fix m3 all nvt temp {t0} {t1} {tdamp_lmp}")
        elif ensemble == "npt":
            bstat = p.get("barostat") or "nose_hoover"
            if tstat == "langevin" or bstat == "berendsen":
                # mixed ensembles: langevin + press/berendsen
                if tstat == "langevin":
                    lines += ["fix m3 all nve",
                              f"fix m3t all langevin {t0} {t1} {tdamp_lmp} {seed}"]
                else:
                    lines.append(f"fix m3 all nvt temp {t0} {t1} {tdamp_lmp}")
                if bstat == "berendsen":
                    lines.append(
                        f"fix m3p all press/berendsen iso {ctx.pressure_atm()} "
                        f"{ctx.pressure_atm()} {pdamp_lmp}")
            else:
                lines.append(
                    f"fix m3 all npt temp {t0} {t1} {tdamp_lmp} "
                    f"iso {ctx.pressure_atm()} {ctx.pressure_atm()} {pdamp_lmp}")

    extra_computes, extra_cols = _interaction_block(ctx)
    lines += _thermo_block(ctx, extra_cols=extra_cols, extra_computes=extra_computes)
    dump_lines, _ = _dump_block(ctx)
    lines += dump_lines
    lines.append(f"timestep {ctx.timestep_lammps()}")
    duration = ctx.time_fs("duration")
    if not duration:
        raise ProviderFailure("input_invalid", "input_error",
                              "run task requires parameter 'duration'")
    lines.append(f"run {ctx.steps(duration)}")
    if ensemble in ("nvt", "npt") and (p.get("thermostat") in ("langevin", "berendsen")
                                       or p.get("barostat") == "berendsen"):
        lines += ["unfix m3", "unfix m3t"] + (["unfix m3p"] if "m3p" in " ".join(lines) else [])
    else:
        lines.append("unfix m3")
    lines += _finalize(ctx)
    return lines, True


def deck_soft_pushoff(ctx):
    p = ctx.params
    lines = _preamble(ctx)
    t = ctx.temperature("temperature", 300.0)
    seed = int(p.get("seed") or 12345)
    duration = ctx.time_fs("duration")
    if not duration:
        raise ProviderFailure("input_invalid", "input_error",
                              "run_soft_pushoff requires 'duration'")
    steps = ctx.steps(duration)
    lines += _velocity_if_needed(ctx, t)
    tdamp = 100.0 / FS_PER_TAU if ctx.units == "lj" else 100.0
    lines += [
        "variable m3pref equal ramp(0.0,1.0)",
        # soften pair interactions, ramping to full over the run
        "fix m3soft all adapt 0 pair lj/cut epsilon * * v_m3pref scale yes",
        "fix m3 all nve/limit 0.05",
        f"fix m3t all langevin {t} {t} {tdamp} {seed}",
    ]
    lines += _thermo_block(ctx)
    lines.append(f"timestep {ctx.timestep_lammps()}")
    lines.append(f"run {steps}")
    lines += ["unfix m3soft", "unfix m3", "unfix m3t"]
    lines += _finalize(ctx)
    return lines, False


def deck_deform(ctx):
    p = ctx.params
    lines = _preamble(ctx)
    t = ctx.temperature("temperature")
    seed = int(p.get("seed") or 12345)
    direction = p.get("direction") or "z"
    erate = float(p.get("strain_rate") or 1e-7)  # 1/fs
    max_strain = float(p.get("max_strain") or 0.5)
    dt = ctx.time_fs("timestep", 1.0)
    steps = max(1, int(round(max_strain / (erate * dt))))
    tdamp = ctx.time_fs("tdamp", 100.0)

    lines += _velocity_if_needed(ctx, t)
    every_fs = ctx.time_fs("sampling_interval", 500.0)
    every = max(1, int(round(every_fs / dt)))
    lines += [
        f"fix m3def all deform 1 {direction} erate {erate} remap v units box",
        f"fix m3 all nvt temp {t} {t} {tdamp}",
        f"variable m3strain equal (step*{dt}*{erate})",
        f"variable m3stress equal -p{direction}{direction}",
        f"fix m3out all print {every} \"${{m3strain}} ${{m3stress}}\" "
        f"file stress_strain.csv screen no",
        f"timestep {ctx.timestep_lammps()}",
        f"run {steps}",
        "unfix m3def",
        "unfix m3",
    ]
    lines += _finalize(ctx)
    return lines, False


# ------------------------------------------------------------------ execution

def _run_lammps(ctx, deck_lines, has_trajectory):
    deck_path = ctx.workdir / "in.m3flow"
    deck_path.write_text("\n".join(deck_lines) + "\n")
    exe = _binary(ctx.req)
    with open(ctx.workdir / "stdout.log", "w") as out:
        proc = subprocess.run([exe, "-in", "in.m3flow", "-log", "log.lammps",
                               "-screen", "none"],
                              cwd=ctx.workdir, stdout=out, stderr=out,
                              timeout=24 * 3600)
    log_path = ctx.workdir / "log.lammps"
    log_text = log_path.read_text(errors="replace") if log_path.is_file() else ""
    _classify_failure(proc.returncode, log_text)
    return log_text


def _classify_failure(returncode, log_text):
    tail = "\n".join(log_text.splitlines()[-40:])
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


def _dcd_frames(path):
    try:
        with open(path, "rb") as f:
            head = f.read(96)
        if len(head) < 96 or head[:4] != b"\x54\x00\x00\x00":
            return 0
        return struct.unpack("<i", head[8:12])[0]
    except Exception:
        return 0


def _write_thermo_csv(ctx, thermo, dest="thermo.csv"):
    dt_fs = ctx.time_fs("timestep", 1.0)
    colmap = {"Step": "step", "Temp": "temp_K", "Press": "press_atm",
              "Volume": "vol_A3", "Density": "density_g_cm3", "PotEng": "pe_kcal_mol",
              "KinEng": "ke_kcal_mol", "TotEng": "etotal_kcal_mol",
              "Enthalpy": "enthalpy_kcal_mol", "c_m3_eint": "e_interaction_kcal_mol"}
    out_cols = ["time_fs"]
    keep = []
    for i, c in enumerate(thermo["columns"]):
        if c == "Step":
            keep.append((i, None))
        elif c in colmap:
            keep.append((i, colmap[c]))
            out_cols.append(colmap[c])
    lines = [",".join(out_cols)]
    for row in thermo["rows"]:
        vals = []
        for i, name in keep:
            if name is None:
                vals.append(f"{row[i] * dt_fs:.1f}")
                        # step -> time
            else:
                vals.append(f"{row[i]:.6g}")
        lines.append(",".join(vals))
    Path(ctx.workdir / dest).write_text("\n".join(lines) + "\n")
    return dest, len(thermo["rows"])


# ------------------------------------------------------------------ outputs + validation

def _common_validation(ctx, log_text, has_trajectory, traj_name="traj.dcd"):
    completed = "Total wall time" in log_text
    nan = "nan" in log_text.lower()
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


def _state_artifact(ctx, meta_extra=None):
    meta = dict(ctx.req["inputs"].get("state") or ctx.req["inputs"]["system"]).get("metadata") or {}
    meta = {**meta, **(meta_extra or {})}
    files = {"data": "final.data", "restart": "final.restart", "init": ctx.init_file}
    return artifact("SimulationState", files=files, metadata=meta)


def _run_outputs(ctx, has_trajectory, thermo, request_meta_extra=None):
    outputs = {"state": _state_artifact(ctx, request_meta_extra)}
    if has_trajectory:
        n_frames = _dcd_frames(ctx.workdir / "traj.dcd")
        outputs["trajectory"] = artifact(
            "Trajectory",
            files={"dcd": "traj.dcd", "topology": ctx.data_file},
            metadata={
                "format": "dcd",
                "topology_format": "lammps_data",
                "units": ctx.units,
                "timestep_fs": ctx.time_fs("timestep", 1.0),
                "frame_interval_fs": ctx.time_fs("sampling_interval", 1000.0),
            },
            data={"n_frames": n_frames})
    outputs["log"] = artifact("SimulationLog", files={"log": "log.lammps"})
    if thermo:
        csv, n_rows = _write_thermo_csv(ctx, thermo)
        outputs["thermo"] = artifact(
            "ThermodynamicSeries",
            files={"csv": csv},
            metadata={"ensemble": request_meta_extra.get("ensemble") if request_meta_extra else None,
                      "temperature_K": ctx.temperature("temperature"),
                      "pressure_bar": (ctx.params.get("pressure") or {}).get("value")},
            data={"columns": thermo["columns"], "n_rows": n_rows})
    return outputs


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
    thermo = None if ensemble in ("minimize",) else _parse_thermo(log_text)
    outputs = _run_outputs(ctx, has_traj, thermo, extra)
    validation = _common_validation(ctx, log_text, has_traj)
    return {"outputs": outputs, "validation": validation}


def energy_minimize(req):
    ctx = Ctx(req)
    deck, _ = deck_minimize(ctx)
    log_text = _run_lammps(ctx, deck, False)
    outputs = {
        "state": _state_artifact(ctx, {"ensemble": "minimize"}),
        "log": artifact("SimulationLog", files={"log": "log.lammps"}),
    }
    return {"outputs": outputs,
            "validation": _common_validation(ctx, log_text, False)}


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
        csv, n_rows = _write_thermo_csv(ctx, thermo)
        outputs["thermo"] = artifact("ThermodynamicSeries", files={"csv": csv},
                                     metadata={"ensemble": "soft_pushoff"},
                                     data={"columns": thermo["columns"], "n_rows": n_rows})
    return {"outputs": outputs,
            "validation": _common_validation(ctx, log_text, False)}


def run_deform(req):
    ctx = Ctx(req)
    deck, _ = deck_deform(ctx)
    log_text = _run_lammps(ctx, deck, False)
    # stress_strain.csv written by fix print: "strain stress_atm" per line
    series_path = ctx.workdir / "stress_strain.csv"
    strain, stress_gpa = [], []
    if series_path.is_file():
        for ln in series_path.read_text().splitlines():
            parts = ln.split()
            if len(parts) >= 2 and not ln.startswith("#"):
                try:
                    strain.append(float(parts[0]))
                    stress_gpa.append(float(parts[1]) * 101325e-9 * 1000)  # atm -> GPa
                except ValueError:
                    continue
    outputs = {
        "state": _state_artifact(ctx, {"ensemble": "deform"}),
        "series": artifact(
            "StressStrainSeries",
            files={"csv": "stress_strain_series.csv"},
            metadata={"direction": ctx.params.get("direction") or "z",
                      "stress_unit": "GPa"},
            data={"n_points": len(strain)}),
        "log": artifact("SimulationLog", files={"log": "log.lammps"}),
    }
    # normalize into a real csv with headers
    with open(ctx.workdir / "stress_strain_series.csv", "w") as f:
        f.write("strain,stress_GPa\n")
        for s, g in zip(strain, stress_gpa):
            f.write(f"{s:.6f},{g:.6f}\n")
    return {"outputs": outputs,
            "validation": _common_validation(ctx, log_text, False)}


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
        })
    raise SystemExit(provider.cli())


if __name__ == "__main__":
    cli()
