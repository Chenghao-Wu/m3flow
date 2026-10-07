"""m3flow-analysis: analysis/validation tasks (MDAnalysis + freud + numpy).

Implements the analysis/validation/utility tasks of tasks/analysis/.
Result artifacts carry their payload both as `data` (for conditions and
downstream references) and as a result.json file (portability).

Statistics conventions:
  - `equilibration_fraction` is always the fraction of samples DISCARDED
    from the start of a series; `tail_fraction` is the fraction KEPT at the
    end. Both are validated, and 0 is a legal value (never replaced by a
    default).
  - `sem` is the standard error of the mean of a time series, estimated by
    Flyvbjerg-Petersen blocking with the Lee-Morales-Umrigar optimal-block
    criterion (time-correlated samples are not independent). `std` is the
    sample standard deviation of the samples. A statistic that cannot be
    estimated is null with an explicit status — never NaN.
  - Time axes come from artifact metadata (actual sampling intervals); a
    trajectory without them is rejected rather than given a default.
  - Reduced (lj) inputs produce reduced results unless an explicit physical
    mapping (sigma, tau, ...) is supplied.
"""

from __future__ import annotations

import csv
import json
import math
import shutil
from collections import deque
from pathlib import Path

import numpy as np

from m3flow_provider import (Provider, ProviderFailure, artifact, dumps, verdict)

PROVIDER_VERSION = "0.5.0"
CRITERIA_VERSION = "m3flow-equilibration/2"
SEM_METHOD = "blocking (Flyvbjerg-Petersen; Lee-Morales-Umrigar optimal block)"
KCAL_MOL_A2_TO_MJ_M2 = 4184.0 / 6.02214076e23 / 1e-20 * 1e3  # 694.77


def _engine():
    """Versions of every numerical library a task may use (absent ones are
    reported as such, so tasks that do not need them stay cacheable)."""
    desc = {"name": "m3flow-analysis-stack", "numpy": np.__version__}
    for mod, key in (("MDAnalysis", "mdanalysis"), ("freud", "freud"), ("ase", "ase")):
        try:
            desc[key] = __import__(mod).__version__
        except Exception:
            desc[key] = "absent"
    desc["version"] = ",".join(f"{k}={desc[k]}" for k in
                               ("numpy", "mdanalysis", "freud", "ase"))
    return desc


# ------------------------------------------------------------------ parameters

def _bad(msg):
    return ProviderFailure("input_invalid", "input_error", msg)


def _num(p, key, default, lo=None, hi=None, lo_open=False, hi_open=False):
    """Numeric parameter; None -> default (0 stays 0); range-checked."""
    v = p.get(key)
    if v is None:
        v = default
    if isinstance(v, dict):
        v = v.get("value")
    try:
        v = float(v)
    except (TypeError, ValueError):
        raise _bad(f"parameter '{key}' must be a number (got {v!r})")
    if not math.isfinite(v):
        raise _bad(f"parameter '{key}' must be finite")
    if lo is not None and (v < lo or (lo_open and v == lo)):
        raise _bad(f"parameter '{key}'={v} must be {'>' if lo_open else '>='} {lo}")
    if hi is not None and (v > hi or (hi_open and v == hi)):
        raise _bad(f"parameter '{key}'={v} must be {'<' if hi_open else '<='} {hi}")
    return v


def _int(p, key, default, lo=None):
    v = _num(p, key, default, lo=lo)
    if v != int(v):
        raise _bad(f"parameter '{key}' must be an integer (got {v})")
    return int(v)


def _discard_fraction(p, key="equilibration_fraction", default=0.5):
    return _num(p, key, default, lo=0.0, hi=1.0, hi_open=True)


def _keep_fraction(p, key="tail_fraction", default=0.5):
    return _num(p, key, default, lo=0.0, hi=1.0, lo_open=True)


# ------------------------------------------------------------------ helpers

def _result(req, artifact_type, payload, filename="result.json"):
    Path(filename).write_text(dumps(payload, indent=1))
    return artifact(artifact_type, files={"json": filename}, data=payload)


def _read_csv(path):
    with open(path) as f:
        rows = list(csv.DictReader(f))
    return rows


def _column(rows, name):
    try:
        x = np.array([float(r[name]) for r in rows])
    except KeyError:
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"series lacks column '{name}'; has {list(rows[0].keys()) if rows else 'nothing'}")
    except ValueError as e:
        raise _bad(f"column '{name}' has a non-numeric value: {e}")
    if not np.isfinite(x).all():
        raise ProviderFailure("non_finite_value", "scientific_validation",
                              f"column '{name}' contains NaN/Infinity")
    return x


_UNIT_TAGS = ("K", "atm", "A3", "g_cm3", "kcal_mol", "A", "fs", "tau", "lj")


def _series(rows, quantity, required=True):
    """(values, unit) of the unit-tagged thermo column `<quantity>_<unit>`.

    Unit tags: physical ('g_cm3', 'kcal_mol', ...) or 'lj' (reduced).
    """
    if not rows:
        raise _bad("series is empty")
    prefix = quantity + "_"
    hits = [c for c in rows[0].keys() if c.startswith(prefix)
            and c[len(prefix):] in _UNIT_TAGS]
    if not hits:
        if required:
            raise ProviderFailure(
                "input_invalid", "input_error",
                f"series lacks a '{quantity}_<unit>' column; has {list(rows[0].keys())}")
        return None, None
    col = hits[0]
    return _column(rows, col), col[len(prefix):]


def _time_axis(rows):
    """(time, unit) from a thermo series; must be strictly increasing."""
    t, unit = _series(rows, "time")
    if len(t) > 1 and not np.all(np.diff(t) > 0):
        raise _bad("time axis of the series is not strictly increasing "
                   "(concatenated or restarted series?)")
    return t, unit


def _discard_head(x, fraction):
    """Drop the first `fraction` of the samples; returns (kept, n_dropped)."""
    n = len(x)
    k = int(round(n * fraction))
    if k >= n:
        raise ProviderFailure(
            "insufficient_data", "scientific_validation",
            f"discarding {fraction:g} of {n} samples leaves nothing to analyze")
    return x[k:], k


def _keep_tail(x, fraction):
    """Keep the last `fraction` of the samples; returns (kept, start_index)."""
    n = len(x)
    keep = max(1, int(round(n * fraction))) if n else 0
    return x[n - keep:], n - keep


def blocking_sem(x):
    """Standard error of the mean of a correlated series.

    Flyvbjerg-Petersen blocking: repeatedly average neighbouring pairs and
    take the SEM at each level; the Lee-Morales-Umrigar criterion picks the
    smallest block size B with B^3 > 2 n (sem_B / sem_0)^4. When no level
    qualifies (series short compared with its correlation time) the largest
    estimate over levels with >= 4 blocks is returned and `converged` is
    False — a conservative, flagged answer instead of an optimistic one.
    """
    x = np.asarray(x, dtype=float)
    n = len(x)
    if n < 2:
        return {"sem": None, "status": "insufficient_data", "n": int(n),
                "method": SEM_METHOD}
    levels = []
    y = x
    while len(y) >= 2:
        levels.append((len(y), float(np.std(y, ddof=1) / np.sqrt(len(y)))))
        half = len(y) // 2
        if half < 2:
            break
        y = 0.5 * (y[0:2 * half:2] + y[1:2 * half:2])
    sem0 = levels[0][1]
    std = float(np.std(x, ddof=1))
    if sem0 == 0.0:
        return {"sem": 0.0, "status": "ok", "converged": True, "block_size": 1,
                "n": int(n), "n_eff": float(n), "method": SEM_METHOD}
    best = None
    for i, (_, sem) in enumerate(levels):
        if (2 ** i) ** 3 > 2 * n * (sem / sem0) ** 4:
            best = i
            break
    if best is not None:
        sem = levels[best][1]
        converged = True
    else:
        usable = [s for nb, s in levels if nb >= 4] or [levels[0][1]]
        sem = max(usable)
        best = int(np.argmax([s for _, s in levels]))
        converged = False
    n_eff = (std / sem) ** 2 if sem > 0 else float(n)
    return {"sem": float(sem), "status": "ok", "converged": converged,
            "block_size": int(2 ** best), "n": int(n),
            "n_eff": float(min(n_eff, n)), "method": SEM_METHOD}


def _summary(x):
    """mean / std / sem (+ estimator details) of a sample series."""
    x = np.asarray(x, dtype=float)
    if len(x) == 0:
        raise ProviderFailure("insufficient_data", "scientific_validation",
                              "no samples to average")
    b = blocking_sem(x)
    return {
        "mean": float(np.mean(x)),
        "std": float(np.std(x, ddof=1)) if len(x) > 1 else None,
        "sem": b["sem"],
        "sem_status": b["status"] if b["status"] != "ok" else
                      ("ok" if b.get("converged") else "not_converged"),
        "sem_detail": b,
    }


def _artifact_ref(art):
    """Identity of an input artifact as handed over by the runtime."""
    if not art:
        return None
    return {"id": art.get("id"), "type": art.get("type"),
            "content_hash": art.get("content_hash"),
            "producer": art.get("producer")}


def _universe(traj_input):
    import MDAnalysis as mda
    files = traj_input["files"]
    top, dcd = files.get("topology"), files.get("dcd")
    if not (top and dcd):
        raise ProviderFailure(
            "input_invalid", "input_error",
            "Trajectory artifact needs 'dcd' and 'topology' files")
    # CAS paths are extensionless; stage with proper names so MDAnalysis
    # format detection works.
    # copyfile, not copy: CAS blobs are read-only and the staged copies
    # must not inherit that mode
    shutil.copyfile(top, "topology.data")
    shutil.copyfile(dcd, "traj.dcd")
    try:
        return mda.Universe("topology.data", "traj.dcd", topology_format="DATA")
    except Exception as e:
        raise ProviderFailure(
            "trajectory_corrupt", "input_error",
            f"cannot open trajectory: {e}", recoverable=False)


def _trajectory_time(traj_input):
    """(frame interval, time unit) from Trajectory metadata (actual stride)."""
    meta = traj_input.get("metadata") or {}
    units = meta.get("units", "real")
    if units == "lj" and meta.get("frame_interval_tau") is not None:
        return float(meta["frame_interval_tau"]), "tau"
    if meta.get("frame_interval_fs") is None:
        raise _bad("Trajectory metadata lacks 'frame_interval_fs'; the time axis "
                   "cannot be inferred (register the trajectory with its sampling "
                   "interval)")
    dt = float(meta["frame_interval_fs"])
    if units == "lj":  # older lj trajectories: undo the 1 tau = 1000 fs convention
        return dt / 1000.0, "tau"
    return dt, "fs"


def _length_unit(traj_input):
    meta = traj_input.get("metadata") or {}
    return "sigma" if meta.get("units") == "lj" else "angstrom"


def _tail_start(n_frames, discard_fraction):
    start = int(round(n_frames * discard_fraction))
    if start >= n_frames:
        raise ProviderFailure(
            "insufficient_data", "scientific_validation",
            f"discarding {discard_fraction:g} of {n_frames} frames leaves none")
    return start


# ------------------------------------------------------------------ density

def compute_density(req):
    p = req["parameters"]
    rows = _read_csv(req["inputs"]["thermo"]["files"]["csv"])
    rho, unit = _series(rows, "density")
    frac = _discard_fraction(p)
    tail, skipped = _discard_head(rho, frac)
    s = _summary(tail)
    payload = {"value": s["mean"], "std": s["std"], "sem": s["sem"],
               "sem_status": s["sem_status"], "sem_method": SEM_METHOD,
               "unit": "g/cm3" if unit == "g_cm3" else "lj",
               "n_samples": int(len(tail)), "n_skipped": int(skipped),
               "equilibration_fraction": frac}
    return {"outputs": {"result": _result(req, "DensityResult", payload)}}


# ------------------------------------------------------------------ RDF (freud)

def _box_matrix(dimensions):
    """MDAnalysis [a, b, c, alpha, beta, gamma] -> 3x3 matrix whose rows are
    the lattice vectors (MDAnalysis convention, lower triangular)."""
    from MDAnalysis.lib.mdamath import triclinic_vectors
    return np.asarray(triclinic_vectors(np.asarray(dimensions[:6], dtype=float)),
                      dtype=float)


def _freud_box(dimensions):
    """MDAnalysis box -> freud box. freud wants the lattice vectors as
    matrix *columns* (tilt factors are dimensionless, xy = b_x / b_y)."""
    import freud
    return freud.box.Box.from_matrix(_box_matrix(dimensions).T)


def _plane_widths(dimensions):
    """Perpendicular widths of the cell (min-image limit is half of these)."""
    m = _box_matrix(dimensions)
    a, b, c = m
    vol = abs(float(np.dot(a, np.cross(b, c))))
    return np.array([vol / np.linalg.norm(np.cross(b, c)),
                     vol / np.linalg.norm(np.cross(c, a)),
                     vol / np.linalg.norm(np.cross(a, b))])


def compute_rdf(req):
    p = req["parameters"]
    rmax_q = p.get("rmax")
    rmax = float(rmax_q["value"]) if isinstance(rmax_q, dict) else (
        float(rmax_q) if rmax_q is not None else 10.0)
    if rmax <= 0:
        raise _bad("rmax must be positive")
    nbins = _int(p, "nbins", 200, lo=1)
    traj = req["inputs"]["trajectory"]
    u = _universe(traj)
    types_a = p.get("types_a")
    types_b = p.get("types_b")

    import freud
    # Small boxes: clamp r_max to what the box supports (recorded, not silent)
    wmin = float(_plane_widths(u.trajectory[0].dimensions).min())
    note = None
    if rmax > wmin / 2.2:
        rmax_eff = wmin / 2.2
        note = (f"r_max clamped {rmax:.2f} -> {rmax_eff:.2f} "
                f"(smallest cell width {wmin:.1f})")
        rmax = rmax_eff

    sel_a = _type_selection(u, types_a)
    sel_b = _type_selection(u, types_b)
    group_a = sel_a if sel_a is not None else u.atoms
    group_b = sel_b if sel_b is not None else u.atoms
    ia, ib = set(group_a.indices), set(group_b.indices)
    same = ia == ib
    if not same and ia & ib:
        raise _bad("types_a and types_b must select identical or disjoint atom "
                   "sets (partially overlapping sets would count self pairs)")
    # identical sets: let freud exclude i == i pairs and normalize by N - 1
    rdf = freud.density.RDF(bins=nbins, r_max=rmax,
                            normalization_mode="finite_size" if same else "exact")
    n_used = 0
    for ts in u.trajectory:
        if _plane_widths(ts.dimensions).min() < 2.2 * rmax:
            continue  # box too small for this r_max; skip rather than lie
        box = _freud_box(ts.dimensions)
        aq = freud.locality.AABBQuery(box, box.wrap(group_b.positions))
        rdf.compute(aq, None if same else box.wrap(group_a.positions), reset=False)
        n_used += 1
    if n_used == 0:
        raise ProviderFailure("trajectory_corrupt", "input_error",
                              "no usable frames (cell narrower than 2.2 x r_max?)")
    out = {"r": [float(x) for x in rdf.bin_centers],
           "g_r": [float(x) for x in np.nan_to_num(rdf.rdf)],
           "rmax": float(rmax), "nbins": nbins,
           "types_a": types_a, "types_b": types_b,
           "n_frames_used": n_used,
           "note": note,
           "unit": _length_unit(traj)}
    with open("rdf.csv", "w") as f:
        f.write("r,g_r\n")
        for r, g in zip(out["r"], out["g_r"]):
            f.write(f"{r:.5f},{g:.6f}\n")
    art = _result(req, "RDFResult", out)
    art["files"]["csv"] = "rdf.csv"
    return {"outputs": {"result": art}}


def _type_selection(u, types):
    if not types:
        return None
    wanted = {int(t) for t in types}
    # LAMMPS numeric atom types -> MDAnalysis 'types' are strings
    mask = np.array([int(t) in wanted for t in u.atoms.types])
    if not mask.any():
        raise ProviderFailure("input_invalid", "input_error",
                              f"no atoms with types {sorted(wanted)}")
    return u.atoms[mask]


# ------------------------------------------------------------------ chains (Rg / Ree)

def _select(u, selection, what):
    if not selection:
        return u.atoms
    try:
        ag = u.select_atoms(selection)
    except Exception as e:
        raise _bad(f"{what} '{selection}' is not a valid MDAnalysis selection: {e}")
    if len(ag) == 0:
        raise _bad(f"{what} '{selection}' selects no atoms")
    return ag


def _polymer_chains(u, p):
    """Bond-connected molecules within `polymer_selection` (default: all).

    Mixtures, solvents and substrates must be excluded with a selection —
    every selected fragment is interpreted as one polymer chain.
    """
    sel = _select(u, p.get("polymer_selection"), "polymer_selection")
    try:
        frags = sel.fragments
    except Exception as e:
        raise _bad(f"topology has no bond information ({e})")
    chains = [f.intersection(sel) for f in frags]
    chains = [c for c in chains if len(c) >= 2]
    if not chains:
        raise ProviderFailure("input_invalid", "input_error",
                              "selection contains no bond-defined chains (>= 2 atoms)")
    return chains


def _adjacency(atoms):
    """Bond graph restricted to `atoms`: {atom index: set(neighbour indices)}."""
    idx = set(int(i) for i in atoms.indices)
    adj = {i: set() for i in idx}
    for b in atoms.bonds:
        i, j = int(b.indices[0]), int(b.indices[1])
        if i in idx and j in idx:
            adj[i].add(j)
            adj[j].add(i)
    return adj


def _graph_path(adj, start, end):
    """Atom indices along the shortest bonded path start -> end."""
    prev = {start: None}
    queue = deque([start])
    while queue:
        cur = queue.popleft()
        if cur == end:
            break
        for nb in adj[cur]:
            if nb not in prev:
                prev[nb] = cur
                queue.append(nb)
    if end not in prev:
        return None
    path = [end]
    while prev[path[-1]] is not None:
        path.append(prev[path[-1]])
    return path[::-1]


def _chain_end_paths(u, chains, p):
    """Bonded path between the two ends of every chain.

    Ends are either chosen explicitly (`end_selection`, exactly two atoms
    per chain) or are the two termini of a linear backbone graph
    (`backbone_selection`, default the whole chain). Anything that is not a
    simple path — rings, stars, branches, all-atom chains whose terminal
    atoms are hydrogens — needs an explicit end/backbone definition.
    """
    ends_sel = _select(u, p.get("end_selection"), "end_selection") \
        if p.get("end_selection") else None
    bb_sel = _select(u, p.get("backbone_selection"), "backbone_selection") \
        if p.get("backbone_selection") else None
    paths = []
    for k, chain in enumerate(chains):
        if ends_sel is not None:
            ends = [int(i) for i in chain.intersection(ends_sel).indices]
            if len(ends) != 2:
                raise _bad(f"chain {k}: end_selection matches {len(ends)} atoms "
                           "(need exactly 2 per chain)")
            adj = _adjacency(chain)
        else:
            backbone = chain.intersection(bb_sel) if bb_sel is not None else chain
            adj = _adjacency(backbone)
            degrees = {i: len(n) for i, n in adj.items()}
            ends = sorted(i for i, d in degrees.items() if d == 1)
            n_edges = sum(degrees.values()) // 2
            linear = (len(ends) == 2 and max(degrees.values()) <= 2
                      and n_edges == len(adj) - 1)
            if not linear:
                raise _bad(
                    f"chain {k} is not a linear bonded path ({len(ends)} termini, "
                    f"max degree {max(degrees.values())}); define the chain ends "
                    "with end_selection or the backbone with backbone_selection")
        path = _graph_path(adj, ends[0], ends[1])
        if path is None:
            raise _bad(f"chain {k}: the two ends are not bond-connected")
        paths.append(np.array(path, dtype=int))
    return paths


def _end_to_end(positions, path, dimensions):
    """|R_ee| from minimum-image bond vectors summed along the path (works
    for wrapped or unwrapped coordinates and any triclinic cell)."""
    from MDAnalysis.lib.distances import minimize_vectors
    vecs = positions[path[1:]] - positions[path[:-1]]
    if dimensions is not None and np.all(np.asarray(dimensions[:3]) > 0):
        vecs = minimize_vectors(vecs.astype(np.float32),
                                np.asarray(dimensions, dtype=np.float32))
    return float(np.linalg.norm(vecs.sum(axis=0)))


def compute_rg(req):
    p = req["parameters"]
    traj = req["inputs"]["trajectory"]
    u = _universe(traj)
    frac = _discard_fraction(p)
    chains = _polymer_chains(u, p)
    start = _tail_start(len(u.trajectory), frac)
    values = []
    for ts in u.trajectory[start:]:
        values.append([c.radius_of_gyration() for c in chains])
    arr = np.array(values)  # frames x chains
    per_frame = arr.mean(axis=1)
    s = _summary(per_frame)  # frames are the correlated samples
    payload = {"value": s["mean"], "sem": s["sem"], "sem_status": s["sem_status"],
               "sem_method": SEM_METHOD,
               "std_between_chains": float(arr.std(ddof=1)) if arr.size > 1 else None,
               "unit": _length_unit(traj), "mass_weighted": True,
               "n_chains": len(chains), "n_frames": int(arr.shape[0]),
               "equilibration_fraction": frac,
               "drift": float(per_frame[-1] - per_frame[0]) if len(per_frame) > 1 else 0.0}
    return {"outputs": {"result": _result(req, "RgResult", payload)}}


def compute_ree(req):
    p = req["parameters"]
    traj = req["inputs"]["trajectory"]
    u = _universe(traj)
    frac = _discard_fraction(p)
    chains = _polymer_chains(u, p)
    paths = _chain_end_paths(u, chains, p)
    start = _tail_start(len(u.trajectory), frac)
    per_frame, per_frame_sq = [], []
    for ts in u.trajectory[start:]:
        pos = u.atoms.positions
        ree = [_end_to_end(pos, path, ts.dimensions) for path in paths]
        per_frame.append(float(np.mean(ree)))
        per_frame_sq.append(float(np.mean(np.square(ree))))
    s = _summary(np.array(per_frame))
    payload = {"value": s["mean"], "sem": s["sem"], "sem_status": s["sem_status"],
               "sem_method": SEM_METHOD,
               "mean_square": float(np.mean(per_frame_sq)),
               "unit": _length_unit(traj),
               "end_definition": ("end_selection" if p.get("end_selection") else
                                  "termini of backbone_selection" if p.get("backbone_selection")
                                  else "termini of the bond graph"),
               "n_chains": len(chains), "n_frames": len(per_frame),
               "n_samples": len(per_frame) * len(chains),
               "equilibration_fraction": frac}
    return {"outputs": {"result": _result(req, "ReeResult", payload)}}


# ------------------------------------------------------------------ MSD / diffusion

def msd_fft(x):
    """Time-origin-averaged MSD for lags 0..T-1 of x[T, N, 3] (Kneller /
    Calandrini FFT algorithm; identical to the direct multi-origin average
    up to round-off, at O(T log T) instead of O(T^2))."""
    x = np.asarray(x, dtype=float)
    T = x.shape[0]
    d = np.square(x).sum(axis=-1)                       # T x N
    d = np.concatenate([d, np.zeros((1,) + d.shape[1:])])
    q = 2.0 * d[:T].sum(axis=0)
    s1 = np.empty((T,) + d.shape[1:])
    for m in range(T):
        q = q - d[m - 1] - d[T - m]
        s1[m] = q / (T - m)
    f = np.fft.rfft(x, n=2 * T, axis=0)
    ac = np.fft.irfft(f * f.conj(), axis=0)[:T].sum(axis=-1)   # T x N
    s2 = ac / (T - np.arange(T))[:, None]
    return (s1 - 2.0 * s2).mean(axis=1)


def _msd_lags(arr, max_lag_frac):
    n_frames = arr.shape[0]
    if n_frames < 2:
        raise ProviderFailure("insufficient_data", "scientific_validation",
                              f"MSD needs >= 2 frames (got {n_frames})")
    max_lag = max(1, min(n_frames - 1, int(n_frames * max_lag_frac)))
    msd = msd_fft(arr)
    return np.arange(1, max_lag + 1), msd[1:max_lag + 1]


def compute_msd(req):
    p = req["parameters"]
    traj = req["inputs"]["trajectory"]
    frame_dt, time_unit = _trajectory_time(traj)
    u = _universe(traj)
    by_mol = bool(p.get("com_by_molecule") or False)
    max_lag_frac = _num(p, "max_lag_fraction", 0.5, lo=0.0, hi=1.0, lo_open=True)

    groups = u.atoms.fragments if by_mol else None
    # positions: frames x particles x 3 (unwrapped per dump_modify)
    series = []
    for ts in u.trajectory:
        if by_mol:
            series.append([g.center_of_mass() for g in groups])
        else:
            series.append(u.atoms.positions.copy())
    arr = np.array(series, dtype=float)
    lag_frames, msd = _msd_lags(arr, max_lag_frac)
    lags = [float(k * frame_dt) for k in lag_frames]
    length = _length_unit(traj)
    payload = {"lag": lags, "time_unit": time_unit, "msd": [float(m) for m in msd],
               "unit": "angstrom2" if length == "angstrom" else "sigma2",
               "frame_interval": frame_dt, "algorithm": "fft",
               "com_by_molecule": by_mol, "n_frames": int(arr.shape[0])}
    if time_unit == "fs":
        payload["lag_fs"] = lags
    art = _result(req, "MSDResult", payload)
    with open("msd.csv", "w") as f:
        f.write(f"lag_{time_unit},msd\n")
        for l, m in zip(lags, payload["msd"]):
            f.write(f"{l:.6g},{m:.8g}\n")
    art["files"]["csv"] = "msd.csv"
    return {"outputs": {"result": art}}


def _fit_window(n, f0, f1):
    if not (0.0 <= f0 < f1 <= 1.0):
        raise _bad(f"fit window fractions must satisfy 0 <= start < end <= 1 "
                   f"(got {f0}, {f1})")
    i0 = int(n * f0)
    i1 = max(int(n * f1), i0 + 2)
    if i1 > n:
        raise ProviderFailure("insufficient_data", "scientific_validation",
                              f"fit window needs >= 2 points; MSD has {n}")
    return i0, i1


def _msd_axes(msd):
    lag = msd.get("lag", msd.get("lag_fs"))
    if lag is None or "msd" not in msd:
        raise ProviderFailure("input_invalid", "input_error",
                              "MSDResult artifact lacks lag/msd data")
    time_unit = msd.get("time_unit") or ("fs" if "lag_fs" in msd else None)
    lag, y = np.asarray(lag, dtype=float), np.asarray(msd["msd"], dtype=float)
    if len(lag) != len(y):
        raise _bad("MSD lag and value arrays differ in length")
    if len(lag) > 1 and not np.all(np.diff(lag) > 0):
        raise _bad("MSD lag axis is not strictly increasing")
    return lag, y, time_unit


def fit_diffusion(req):
    msd = req["inputs"]["msd"].get("data")
    if not msd:
        raise ProviderFailure("input_invalid", "input_error",
                              "MSDResult artifact lacks data payload")
    p = req["parameters"]
    f0 = _num(p, "fit_start_fraction", 0.2, lo=0.0, hi=1.0)
    f1 = _num(p, "fit_end_fraction", 0.8, lo=0.0, hi=1.0)
    lag, y, time_unit = _msd_axes(msd)
    i0, i1 = _fit_window(len(lag), f0, f1)
    slope, intercept = np.polyfit(lag[i0:i1], y[i0:i1], 1)
    d_raw = slope / 6.0
    msd_unit = msd.get("unit")
    payload = {"slope": float(slope), "msd_unit": msd_unit, "time_unit": time_unit,
               "fit_window": [float(lag[i0]), float(lag[i1 - 1])],
               "intercept": float(intercept)}
    if msd_unit == "angstrom2" and time_unit == "fs":
        # 1 A^2/fs = 1e-16 cm^2 / 1e-15 s = 0.1 cm^2/s
        payload.update({"value": float(d_raw * 0.1), "unit": "cm2/s",
                        "slope_A2_per_fs": float(slope),
                        "fit_window_fs": payload["fit_window"]})
    elif msd_unit == "sigma2" and time_unit == "tau":
        sigma, tau = p.get("sigma"), p.get("tau")
        if sigma is not None and tau is not None:
            sigma_a = float(sigma["value"]) if isinstance(sigma, dict) else float(sigma)
            tau_fs = float(tau["value"]) if isinstance(tau, dict) else float(tau)
            if sigma_a <= 0 or tau_fs <= 0:
                raise _bad("sigma and tau must be positive")
            payload.update({"value": float(d_raw * sigma_a ** 2 / tau_fs * 0.1),
                            "unit": "cm2/s", "reduced_value": float(d_raw),
                            "mapping": {"sigma_angstrom": sigma_a, "tau_fs": tau_fs}})
        elif sigma is None and tau is None:
            payload.update({"value": float(d_raw), "unit": "sigma2/tau",
                            "note": "reduced units; pass sigma and tau to map to cm2/s"})
        else:
            raise _bad("mapping reduced diffusion needs both 'sigma' and 'tau'")
    else:
        raise _bad(f"unsupported MSD units (msd {msd_unit!r}, time {time_unit!r})")
    return {"outputs": {"result": _result(req, "DiffusionResult", payload)}}


# ------------------------------------------------------------------ series fan-in / fits

def collect_thermo_series(req):
    series = req["inputs"]["series"]
    if not isinstance(series, list):
        series = [series]
    frac = _discard_fraction(req["parameters"])
    points = []
    for art in series:
        if (art.get("metadata") or {}).get("units") == "lj":
            raise _bad("collect_thermo_series supports physical (real-unit) series "
                       "only; reduced lj series would be mislabeled")
        rows = _read_csv(art["files"]["csv"])
        t_meta = (art.get("metadata") or {}).get("temperature_K")
        temp = float(t_meta) if t_meta is not None else float(np.mean(_column(rows, "temp_K")))
        vol = float(np.mean(_discard_head(_column(rows, "vol_A3"), frac)[0]))
        rho = float(np.mean(_discard_head(_column(rows, "density_g_cm3"), frac)[0]))
        e = float(np.mean(_discard_head(_column(rows, "etotal_kcal_mol"), frac)[0]))
        points.append({"temperature_K": temp, "volume_A3": vol,
                       "density_g_cm3": rho, "energy_kcal_mol": e})
    points.sort(key=lambda p: p["temperature_K"])
    with open("series.csv", "w") as f:
        f.write("temperature_K,volume_A3,density_g_cm3,energy_kcal_mol\n")
        for pt in points:
            f.write(f"{pt['temperature_K']:.2f},{pt['volume_A3']:.4f},"
                    f"{pt['density_g_cm3']:.6f},{pt['energy_kcal_mol']:.4f}\n")
    out = artifact("TemperatureSeries", files={"csv": "series.csv"},
                   metadata={"n_points": len(points), "equilibration_fraction": frac},
                   data={"points": points})
    return {"outputs": {"series": out}}


def _series_points(req):
    art = req["inputs"]["series"]
    if art.get("data") and art["data"].get("points"):
        pts = art["data"]["points"]
        return (np.array([p["temperature_K"] for p in pts]),
                np.array([p["volume_A3"] for p in pts]),
                np.array([p["density_g_cm3"] for p in pts]))
    rows = _read_csv(art["files"]["csv"])
    return (_column(rows, "temperature_K"), _column(rows, "volume_A3"),
            _column(rows, "density_g_cm3"))


def fit_cte(req):
    t, v, _ = _series_points(req)
    p = req["parameters"]
    tref_q = p.get("reference_temperature")
    tref = float(tref_q["value"]) if isinstance(tref_q, dict) else float(np.median(t))
    order = np.argsort(np.abs(t - tref))
    k = max(_int(p, "min_points", 3, lo=2), 3)
    if len(t) < k:
        raise ProviderFailure("insufficient_data", "scientific_validation",
                              f"CTE fit needs >= {k} temperatures, got {len(t)}")
    sel = np.sort(order[:k])
    slope, intercept = np.polyfit(t[sel], v[sel], 1)
    v_ref = slope * tref + intercept
    alpha = slope / v_ref
    payload = {"value": float(alpha), "unit": "1/K",
               "reference_temperature_K": float(tref),
               "dVdT_A3_per_K": float(slope), "V_ref_A3": float(v_ref),
               "n_points": int(len(sel))}
    return {"outputs": {"result": _result(req, "CTEResult", payload)}}


def fit_tg(req):
    t, v, _ = _series_points(req)
    k = _int(req["parameters"], "min_points_per_branch", 3, lo=2)
    order = np.argsort(t)
    t, v = t[order], v[order]
    best = None
    for i in range(k, len(t) - k + 1):
        s1, b1 = np.polyfit(t[:i], v[:i], 1)
        s2, b2 = np.polyfit(t[i:], v[i:], 1)
        sse = float(np.sum((np.polyval([s1, b1], t[:i]) - v[:i]) ** 2)
                    + np.sum((np.polyval([s2, b2], t[i:]) - v[i:]) ** 2))
        if abs(s2 - s1) < 1e-12:
            continue
        tg = (b1 - b2) / (s2 - s1)
        if not (t[0] <= tg <= t[-1]):
            continue
        if best is None or sse < best[0]:
            best = (sse, tg, s1, s2, i)
    if best is None:
        raise ProviderFailure(
            "validation_failed", "scientific_validation",
            "no valid bilinear split: sweep may not bracket Tg")
    sse, tg, s1, s2, i = best
    payload = {"value": float(tg), "unit": "K",
               "slope_below_A3_per_K": float(s1), "slope_above_A3_per_K": float(s2),
               "split_index": int(i), "sse": sse,
               "sweep_range_K": [float(t[0]), float(t[-1])]}
    return {"outputs": {"result": _result(req, "TgResult", payload)}}


# ------------------------------------------------------------------ equilibration gate

PASSED, FAILED, INSUFFICIENT, ERROR = "passed", "failed", "insufficient_data", "error"
MIN_TAIL_SAMPLES = 8


def _trend_check(t, x, sigma_mult):
    """Is there a statistically significant linear trend in x(t)?

    z = |slope| / SE(slope), with SE inflated by the blocking estimate of
    the residuals' correlation (an i.i.d. SE would flag every correlated
    fluctuation). A noiseless ramp has SE = 0 and fails.
    """
    if len(x) < MIN_TAIL_SAMPLES:
        return INSUFFICIENT, {"n": int(len(x))}
    slope, intercept = np.polyfit(t, x, 1)
    resid = x - (slope * t + intercept)
    span = float(t[-1] - t[0])
    change = abs(float(slope)) * span
    scale = max(float(np.max(np.abs(x))), 1e-300)
    sxx = float(np.sum((t - t.mean()) ** 2))
    if change <= 1e-12 * scale:
        return PASSED, {"slope": float(slope), "z": 0.0, "total_change": change}
    resid_std = float(np.std(resid, ddof=2)) if len(x) > 2 else 0.0
    se_iid = resid_std / math.sqrt(sxx) if sxx > 0 else 0.0
    b = blocking_sem(resid)
    iid_sem = resid_std / math.sqrt(len(x)) if resid_std > 0 else 0.0
    inflation = max(1.0, (b["sem"] or 0.0) / iid_sem) if iid_sem > 0 else 1.0
    se = se_iid * inflation
    z = float("inf") if se == 0 else abs(float(slope)) / se
    detail = {"slope": float(slope), "slope_se": se, "z": None if math.isinf(z) else z,
              "total_change": change, "correlation_inflation": inflation,
              "threshold_sigma": sigma_mult}
    return (FAILED if z > sigma_mult else PASSED), detail


def _shift_check(x, sigma_mult):
    """Mean of the second half of the tail vs the first half, in combined
    blocked standard errors."""
    if len(x) < MIN_TAIL_SAMPLES:
        return INSUFFICIENT, {"n": int(len(x))}
    h = len(x) // 2
    a, b = x[:h], x[h:]
    sa, sb = blocking_sem(a)["sem"], blocking_sem(b)["sem"]
    if sa is None or sb is None:
        return INSUFFICIENT, {"n": int(len(x))}
    shift = abs(float(b.mean() - a.mean()))
    se = math.hypot(sa, sb)
    if se == 0:
        status = PASSED if shift <= 1e-12 * max(abs(float(x.mean())), 1e-300) else FAILED
        return status, {"shift": shift, "se": 0.0}
    z = shift / se
    return (PASSED if z < sigma_mult else FAILED), {
        "shift": shift, "se": se, "z": z, "threshold_sigma": sigma_mult}


def _structural_rg_check(traj, keep_frac, drift_threshold, p):
    try:
        u = _universe(traj)
        chains = _polymer_chains(u, p)
    except ProviderFailure as e:
        return ERROR, {"error": e.message}
    except Exception as e:  # any read failure is evidence missing, not a pass
        return ERROR, {"error": f"{type(e).__name__}: {e}"}
    n = len(u.trajectory)
    start = n - max(1, int(round(n * keep_frac)))
    rg = np.array([np.mean([c.radius_of_gyration() for c in chains])
                   for _ in u.trajectory[start:]])
    if len(rg) < 4:
        return INSUFFICIENT, {"n_frames": int(len(rg))}
    s_rg, _ = np.polyfit(np.arange(len(rg)), rg, 1)
    drift = abs(float(s_rg)) * len(rg) / float(np.mean(rg))
    limit = drift_threshold * 10
    return (PASSED if drift < limit else FAILED), {
        "rg_relative_drift": drift, "threshold": limit, "n_frames": int(len(rg)),
        "n_chains": len(chains)}


def check_polymer_equilibration(req):
    """Thermodynamic stationarity gate for one sampling window.

    Each check is passed / failed / insufficient_data / error; the report is
    `equilibrated` only when every required check passed. Missing evidence
    never counts as a pass. The report is bound to the evidence it examined
    (artifact ids + content hashes + producing task run) and, when given, to
    the state it certifies.
    """
    p = req["parameters"]
    thermo_in = req["inputs"]["thermo"]
    rows = _read_csv(thermo_in["files"]["csv"])
    drift_threshold = _num(p, "density_drift_threshold", 0.01, lo=0.0, lo_open=True)
    sigma_mult = _num(p, "stationarity_sigma", 3.0, lo=0.0, lo_open=True)
    tail_frac = _keep_fraction(p)

    time, time_unit = _time_axis(rows)
    tt, start = _keep_tail(time, tail_frac)
    checks, details = {}, {}
    metrics = {"stationarity_sigma": sigma_mult,
               "density_drift_threshold": drift_threshold,
               "n_tail_samples": int(len(tt))}

    rho, rho_unit = _series(rows, "density")
    tail = rho[start:]
    mean_rho = float(np.mean(tail))
    metrics["density_mean"] = mean_rho
    metrics["density_unit"] = rho_unit
    if rho_unit == "g_cm3":
        metrics["density_mean_g_cm3"] = mean_rho
    if len(tail) < MIN_TAIL_SAMPLES:
        checks["density_drift"] = INSUFFICIENT
    else:
        slope, _ = np.polyfit(tt, tail, 1)
        total_drift = abs(float(slope)) * float(tt[-1] - tt[0]) / abs(mean_rho) \
            if mean_rho else float("inf")
        metrics["density_relative_drift"] = None if math.isinf(total_drift) else total_drift
        checks["density_drift"] = PASSED if total_drift < drift_threshold else FAILED
    checks["density_stationarity"], details["density_stationarity"] = \
        _shift_check(tail, sigma_mult)

    observables = p.get("observables") or ["energy", "volume"]
    columns = {"energy": "etotal", "potential_energy": "pe", "volume": "vol"}
    for obs in observables:
        if obs not in columns:
            raise _bad(f"unknown observable '{obs}' (use {', '.join(columns)})")
        x, unit = _series(rows, columns[obs], required=False)
        if x is None:
            checks[f"{obs}_trend"] = INSUFFICIENT
            details[f"{obs}_trend"] = {"error": f"no '{columns[obs]}_*' column"}
            continue
        xt = x[start:]
        checks[f"{obs}_trend"], details[f"{obs}_trend"] = _trend_check(tt, xt, sigma_mult)
        checks[f"{obs}_stationarity"], details[f"{obs}_stationarity"] = \
            _shift_check(xt, sigma_mult)
        metrics[f"{obs}_mean"] = float(np.mean(xt))
        metrics[f"{obs}_unit"] = unit

    traj = req["inputs"].get("trajectory")
    if traj:
        checks["rg_stable"], details["rg_stable"] = \
            _structural_rg_check(traj, tail_frac, drift_threshold, p)
        if "rg_relative_drift" in details["rg_stable"]:
            metrics["rg_relative_drift"] = details["rg_stable"]["rg_relative_drift"]

    equilibrated = all(v == PASSED for v in checks.values())
    state_in = req["inputs"].get("state")
    payload = {
        "equilibrated": equilibrated,
        "criteria_version": CRITERIA_VERSION,
        "checks": checks,
        "check_details": details,
        "metrics": metrics,
        "scope": ("stationarity of the listed thermodynamic observables (and Rg "
                  "when a trajectory is given) over this sampling window; not "
                  "evidence of conformational, entanglement or long-time "
                  "dynamical equilibration"),
        "sampling_window": {"time_start": float(tt[0]), "time_end": float(tt[-1]),
                            "time_unit": time_unit,
                            "n_samples": int(len(tt)), "tail_fraction": tail_frac},
        "evidence": {"thermo": _artifact_ref(thermo_in),
                     "trajectory": _artifact_ref(traj)},
        "subject": None if not state_in else {
            "state_id": state_in.get("id"),
            "state_content_hash": state_in.get("content_hash"),
            "state_producer": state_in.get("producer")},
    }
    Path("report.json").write_text(dumps(payload, indent=1))
    out = artifact("EquilibrationReport", files={"json": "report.json"}, data=payload)
    return {"outputs": {"report": out}}


def _report_binds_state(report, state):
    """The report must have examined this state (explicit subject) or
    evidence produced by the same task run as the state."""
    subject = report.get("subject") or {}
    if subject.get("state_content_hash"):
        return subject["state_content_hash"] == state.get("content_hash"), "subject"
    evidence = (report.get("evidence") or {}).get("thermo") or {}
    producer = evidence.get("producer")
    if producer and state.get("producer"):
        return producer == state["producer"], "shared_producer"
    return False, "unbound"


def promote_equilibrated_state(req):
    report_in = req["inputs"]["report"]
    report = report_in.get("data") or {}
    state_in = req["inputs"]["state"]
    require = req["parameters"].get("require_pass")
    if require is not None and not bool(require):
        raise _bad(
            "require_pass=false is no longer supported: an EquilibratedState "
            "must only exist behind a passing, bound EquilibrationReport. To "
            "continue without certification, use the SimulationState directly.")
    if report.get("criteria_version") != CRITERIA_VERSION:
        raise ProviderFailure(
            "report_not_bound", "scientific_validation",
            f"EquilibrationReport uses criteria {report.get('criteria_version')!r}, "
            f"not {CRITERIA_VERSION!r} (it records no evidence binding); re-run "
            "check_polymer_equilibration")
    bound, how = _report_binds_state(report, state_in)
    if not bound:
        raise ProviderFailure(
            "report_not_bound", "scientific_validation",
            "the EquilibrationReport did not examine this state (no matching "
            "subject fingerprint, and its thermo evidence was produced by a "
            "different task run)",
            details={"binding": how, "subject": report.get("subject"),
                     "evidence": report.get("evidence"),
                     "state": {"id": state_in.get("id"),
                               "content_hash": state_in.get("content_hash"),
                               "producer": state_in.get("producer")}})
    checks = report.get("checks") or {}
    if not report.get("equilibrated") or not checks or \
            any(v != PASSED for v in checks.values()):
        raise ProviderFailure(
            "validation_failed", "scientific_validation",
            "EquilibrationReport says the state is NOT equilibrated; "
            "refusing promotion (run longer or adjust the protocol)",
            details={"checks": checks, "metrics": report.get("metrics")})
    files = {}
    for key, path in state_in["files"].items():
        dest = f"promoted_{key}{Path(path).suffix or '.dat'}"
        shutil.copy(path, dest)
        files[key] = dest
    meta = dict(state_in.get("metadata") or {})
    meta["equilibration"] = {
        "status": "certified",
        "report_id": report_in.get("id"),
        "criteria_version": report.get("criteria_version"),
        "binding": how,
        "checks": checks,
        "scope": report.get("scope"),
        "sampling_window": report.get("sampling_window"),
        "evidence": report.get("evidence"),
        "source_state": {"id": state_in.get("id"),
                         "content_hash": state_in.get("content_hash")},
    }
    out = artifact("EquilibratedState", files=files, metadata=meta,
                   data=state_in.get("data"))
    return {"outputs": {"state": out}}


# ------------------------------------------------------------------ adhesion / modulus

def compute_adhesion(req):
    """Interfacial interaction-energy density -<E_int>/(n A).

    This is the group/group interaction energy per interface area. It is NOT
    the reversible work of adhesion (no entropy, surface reconstruction or
    separation path); the payload says which energy terms it contains.
    """
    p = req["parameters"]
    thermo_in = req["inputs"]["thermo"]
    meta = thermo_in.get("metadata") or {}
    rows = _read_csv(thermo_in["files"]["csv"])
    e, e_unit = _series(rows, "e_interaction", required=False)
    if e is None:
        cols = list(rows[0].keys()) if rows else []
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"series lacks an e_interaction_<unit> column (has {cols}); "
            "run the MD step with the 'interaction' parameter set")
    frac = _discard_fraction(p)
    tail, skipped = _discard_head(e, frac)
    s = _summary(tail)
    n_if = _int(p, "n_interfaces", 1, lo=1)
    reduced = e_unit == "lj"

    area_q = p.get("interface_area")
    normal = p.get("interface_normal") or "z"
    if normal not in ("x", "y", "z"):
        raise _bad("interface_normal must be x, y or z")
    lat = [a for a in "xyz" if a != normal]
    box_area = None
    la, _ = _series(rows, f"l{lat[0]}", required=False)
    lb, _ = _series(rows, f"l{lat[1]}", required=False)
    if la is not None and lb is not None:
        a_series = (la * lb)[skipped:]
        box_area = {"mean": float(a_series.mean()),
                    "relative_std": float(a_series.std() / a_series.mean())
                    if len(a_series) > 1 else 0.0}
    if area_q is not None:
        area = float(area_q["value"]) if isinstance(area_q, dict) else float(area_q)
        area_source = "parameter"
    elif box_area is not None:
        area = box_area["mean"]
        area_source = f"box l{lat[0]} x l{lat[1]} (tail mean)"
    else:
        raise _bad("interface_area not given and the series has no box lengths")
    if area <= 0:
        raise _bad("interface area must be positive")

    scale = 1.0 if reduced else KCAL_MOL_A2_TO_MJ_M2
    w = -s["mean"] / (n_if * area) * scale
    w_sem = None if s["sem"] is None else s["sem"] / (n_if * area) * scale
    interaction = meta.get("interaction") or {}
    payload = {"value": float(w), "sem": w_sem, "sem_status": s["sem_status"],
               "unit": "epsilon/sigma2" if reduced else "mJ/m2",
               "observable": "interaction_energy_per_area",
               "definition": "-<E_int(group_a, group_b)> / (n_interfaces * area)",
               "note": "interaction-energy density, not the reversible work of adhesion",
               "kspace_included": interaction.get("kspace_included"),
               "energy_terms": interaction.get("observable", "unknown (series lacks metadata)"),
               "interaction_energy": s["mean"], "interaction_energy_sem": s["sem"],
               "interaction_energy_unit": "lj" if reduced else "kcal/mol",
               "n_interfaces": n_if, "interface_area": area,
               "interface_area_source": area_source, "interface_normal": normal,
               "box_area": box_area, "group_charges": interaction.get("group_charges"),
               "n_samples": int(len(tail)), "n_skipped": int(skipped)}
    for comp in ("pair", "kspace"):
        x, _ = _series(rows, f"e_interaction_{comp}", required=False)
        if x is not None:
            payload[f"interaction_energy_{comp}"] = float(np.mean(x[skipped:]))
    if not reduced:
        payload["interaction_energy_kcal_mol"] = s["mean"]
        payload["interface_area_A2"] = area
    warnings = []
    if interaction.get("kspace_included") is None:
        warnings.append("thermo series records no interaction metadata; whether "
                        "long-range (kspace) terms are included is unknown")
    out = {"outputs": {"result": _result(req, "AdhesionResult", payload)}}
    if warnings:
        out["warnings"] = warnings
    return out


def fit_modulus(req):
    series = req["inputs"]["series"]
    rows = _read_csv(series["files"]["csv"])
    meta = series.get("metadata") or {}
    col = meta.get("stress_column") or ("stress_GPa" if rows and "stress_GPa" in rows[0]
                                        else "stress_lj")
    unit = "GPa" if col == "stress_GPa" else "lj"
    strain = _column(rows, "strain")
    stress = _column(rows, col)
    max_strain = _num(req["parameters"], "max_strain", 0.05, lo=0.0, lo_open=True)
    mask = (strain > 0) & (strain <= max_strain)
    if mask.sum() < 3:
        raise ProviderFailure("insufficient_data", "scientific_validation",
                              f"only {int(mask.sum())} points below max_strain={max_strain}")
    slope, intercept = np.polyfit(strain[mask], stress[mask], 1)
    payload = {"value": float(slope), "unit": unit,
               "fit_max_strain": max_strain, "n_points": int(mask.sum()),
               "intercept": float(intercept)}
    if unit == "GPa":
        payload["intercept_GPa"] = float(intercept)
    return {"outputs": {"result": _result(req, "ModulusResult", payload)}}


# ------------------------------------------------------------------ species MSD / Arrhenius

def compute_species_msd(req):
    """Species-resolved MSD from an extxyz trajectory (mlip provider format).

    v1 supports trajectories written as unwrapped extxyz frame series plus a
    topology structure file (cif). LAMMPS dcd inputs are rejected with a
    structured error.
    """
    from ase.io import read as ase_read

    traj_inp = req["inputs"]["trajectory"]
    meta = traj_inp.get("metadata") or {}
    files = traj_inp["files"]
    fmt = meta.get("format")
    if fmt != "extxyz" or "extxyz" not in files:
        raise ProviderFailure(
            "input_invalid", "input_error",
            "compute_species_msd v1 requires an extxyz Trajectory "
            "(metadata.format == 'extxyz'); got format=%r" % fmt)
    specie = req["parameters"].get("specie")
    if not specie:
        raise ProviderFailure("input_invalid", "input_error",
                              "parameter 'specie' is required")
    max_lag_frac = _num(req["parameters"], "max_lag_fraction", 0.5,
                        lo=0.0, hi=1.0, lo_open=True)
    frame_dt_fs, _ = _trajectory_time(traj_inp)

    frames = ase_read(files["extxyz"], index=":")
    if not frames:
        raise ProviderFailure("engine_crash", "engine_error",
                              "extxyz trajectory has no frames")
    symbols = frames[0].get_chemical_symbols()
    idx = [i for i, s in enumerate(symbols) if s == specie]
    if not idx:
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"specie '{specie}' not present; available: {sorted(set(symbols))}")
    arr = np.array([f.get_positions()[idx] for f in frames], dtype=float)
    lag_frames, msd = _msd_lags(arr, max_lag_frac)
    lags = [float(k * frame_dt_fs) for k in lag_frames]
    msd = [float(m) for m in msd]
    payload = {"lag": lags, "lag_fs": lags, "time_unit": "fs", "msd": msd,
               "unit": "angstrom2", "algorithm": "fft",
               "specie": specie, "n_species_atoms": len(idx),
               "n_frames": int(arr.shape[0])}
    art = _result(req, "MSDResult", payload)
    art["metadata"] = {"specie": specie}
    if meta.get("temperature_K") is not None:
        art["metadata"]["temperature_K"] = float(meta["temperature_K"])
    with open("msd.csv", "w") as f:
        f.write("lag_fs,msd\n")
        for l, m in zip(lags, msd):
            f.write(f"{l:.6g},{m:.8g}\n")
    art["files"]["csv"] = "msd.csv"
    return {"outputs": {"result": art},
            "validation": [verdict("trajectory_readable", True),
                           verdict("no_nan", bool(np.isfinite(msd).all()))]}


def fit_arrhenius(req):
    """Per-temperature Einstein fits over fanned-in MSDResults, then Arrhenius."""
    series = req["inputs"]["msd_series"]
    if not isinstance(series, list):
        series = [series]
    p = req["parameters"]
    f0 = _num(p, "fit_start_fraction", 0.2, lo=0.0, hi=1.0)
    f1 = _num(p, "fit_end_fraction", 0.8, lo=0.0, hi=1.0)
    min_points = _int(p, "min_points", 4, lo=2)
    KB_EV = 8.617333262e-5  # eV/K

    points = []
    for art in series:
        data = art.get("data") or {}
        meta = art.get("metadata") or {}
        temp = meta.get("temperature_K")
        if temp is None:
            raise ProviderFailure(
                "input_invalid", "input_error",
                "each MSDResult needs data.lag/msd and metadata.temperature_K")
        lag, y, time_unit = _msd_axes(data)
        if data.get("unit") != "angstrom2" or time_unit != "fs":
            raise _bad("fit_arrhenius needs physical MSDs (angstrom2 vs fs)")
        i0, i1 = _fit_window(len(lag), f0, f1)
        slope, _ = np.polyfit(lag[i0:i1], y[i0:i1], 1)
        d_cm2_s = slope / 6.0 * 0.1
        points.append({"temperature_K": float(temp), "D_cm2_s": float(d_cm2_s)})
    points.sort(key=lambda x: x["temperature_K"])
    if len(points) < min_points:
        raise ProviderFailure(
            "convergence_failed", "validation_failed",
            f"need >= {min_points} temperatures, got {len(points)}",
            recoverable=False)
    if any(pt["D_cm2_s"] <= 0 for pt in points):
        raise ProviderFailure(
            "convergence_failed", "validation_failed",
            "non-positive diffusivity in series", recoverable=False)
    inv_t = np.array([1.0 / pt["temperature_K"] for pt in points])
    ln_d = np.log([pt["D_cm2_s"] for pt in points])
    slope, intercept = np.polyfit(inv_t, ln_d, 1)
    pred = slope * inv_t + intercept
    ss_res = float(((ln_d - pred) ** 2).sum())
    ss_tot = float(((ln_d - ln_d.mean()) ** 2).sum())
    r2 = 1.0 - ss_res / ss_tot if ss_tot > 0 else 1.0
    ea = -slope * KB_EV
    d0 = float(np.exp(intercept))
    payload = {"activation_energy": float(ea), "activation_energy_unit": "eV",
               "d0": d0, "d0_unit": "cm2/s", "r_squared": r2,
               "diffusivities": points}
    art = _result(req, "ArrheniusResult", payload)
    with open("arrhenius.csv", "w") as f:
        f.write("temperature_K,D_cm2_s\n")
        for pt in points:
            f.write(f"{pt['temperature_K']:.1f},{pt['D_cm2_s']:.6e}\n")
    art["files"]["csv"] = "arrhenius.csv"
    return {"outputs": {"result": art},
            "validation": [
                verdict("enough_temperatures", len(points) >= min_points),
                verdict("positive_diffusivities",
                        all(pt["D_cm2_s"] > 0 for pt in points))]}


# ------------------------------------------------------------------ plumbing

def cli():
    provider = Provider(
        name="analysis",
        version=PROVIDER_VERSION,
        engine=_engine,
        tasks={
            "compute_density": compute_density,
            "compute_rdf": compute_rdf,
            "compute_rg": compute_rg,
            "compute_ree": compute_ree,
            "compute_msd": compute_msd,
            "fit_diffusion": fit_diffusion,
            "collect_thermo_series": collect_thermo_series,
            "fit_cte": fit_cte,
            "fit_tg": fit_tg,
            "check_polymer_equilibration": check_polymer_equilibration,
            "promote_equilibrated_state": promote_equilibrated_state,
            "compute_adhesion": compute_adhesion,
            "fit_modulus": fit_modulus,
            "compute_species_msd": compute_species_msd,
            "fit_arrhenius": fit_arrhenius,
        })
    raise SystemExit(provider.cli())


if __name__ == "__main__":
    cli()
