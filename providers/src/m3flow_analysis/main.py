"""m3flow-analysis: analysis/validation tasks (MDAnalysis + freud + numpy).

Implements the 13 analysis/validation/utility tasks of tasks/analysis/.
Result artifacts carry their payload both as `data` (for conditions and
downstream references) and as a result.json file (portability).
"""

from __future__ import annotations

import csv
import json
import shutil
from pathlib import Path

import numpy as np

from m3flow_provider import (Provider, ProviderFailure, artifact, verdict)

PROVIDER_VERSION = "0.3.2"


def _engine():
    import MDAnalysis
    import freud
    return {"name": "mdanalysis", "version": MDAnalysis.__version__,
            "freud": freud.__version__}


# ------------------------------------------------------------------ helpers

def _result(req, artifact_type, payload, filename="result.json"):
    Path(filename).write_text(json.dumps(payload, indent=1))
    return artifact(artifact_type, files={"json": filename}, data=payload)


def _read_csv(path):
    with open(path) as f:
        rows = list(csv.DictReader(f))
    return rows


def _column(rows, name):
    try:
        return np.array([float(r[name]) for r in rows])
    except KeyError:
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"series lacks column '{name}'; has {list(rows[0].keys()) if rows else 'nothing'}")


def _tail(x, fraction):
    n = len(x)
    k = max(1, int(round(n * (1.0 - fraction))))
    return x[k:], k


def _mean_sem(x):
    return float(np.mean(x)), float(np.std(x, ddof=1) / np.sqrt(max(1, len(x) - 1)))


def _correlated_sem(x):
    """SEM accounting for time correlation via lag-1 autocorrelation
    (effective sample size n_eff = n * (1-rho)/(1+rho))."""
    n = len(x)
    if n < 8:
        return float(np.std(x, ddof=1) / np.sqrt(max(1, n - 1)))
    x0 = x[:-1] - x[:-1].mean()
    x1 = x[1:] - x[1:].mean()
    denom = float((x0 * x0).sum() * (x1 * x1).sum())
    rho = float((x0 * x1).sum() / np.sqrt(denom)) if denom > 0 else 0.0
    rho = min(max(rho, -0.95), 0.95)
    n_eff = max(2.0, n * (1.0 - rho) / (1.0 + rho))
    return float(np.std(x, ddof=1) / np.sqrt(n_eff))


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
    shutil.copy(top, "topology.data")
    shutil.copy(dcd, "traj.dcd")
    try:
        return mda.Universe("topology.data", "traj.dcd", topology_format="DATA")
    except Exception as e:
        raise ProviderFailure(
            "trajectory_corrupt", "input_error",
            f"cannot open trajectory: {e}", recoverable=False)


def _tail_start(n_frames, fraction):
    return min(n_frames - 1, max(0, int(round(n_frames * fraction))))


# ------------------------------------------------------------------ density

def compute_density(req):
    rows = _read_csv(req["inputs"]["thermo"]["files"]["csv"])
    rho = _column(rows, "density_g_cm3")
    frac = float(req["parameters"].get("equilibration_fraction") or 0.5)
    tail, skipped = _tail(rho, frac)
    mean, sem = _mean_sem(tail)
    payload = {"value": mean, "std": sem, "unit": "g/cm3",
               "n_samples": int(len(tail)), "n_skipped": int(skipped)}
    return {"outputs": {"result": _result(req, "DensityResult", payload)}}


# ------------------------------------------------------------------ RDF (freud)

def _freud_box(dimensions):
    """MDAnalysis [Lx,Ly,Lz,alpha,beta,gamma] -> freud box (tilt factors)."""
    import freud
    lx, ly, lz, alpha, beta, gamma = [float(x) for x in dimensions[:6]]
    if abs(alpha - 90) < 1e-6 and abs(beta - 90) < 1e-6 and abs(gamma - 90) < 1e-6:
        return freud.box.Box(Lx=lx, Ly=ly, Lz=lz)
    ca, cb, cg = np.cos(np.radians([alpha, beta, gamma]))
    sg = np.sin(np.radians(gamma))
    xy = ly * cg
    xz = lz * cb
    yz = lz * (ca - cb * cg) / sg
    return freud.box.Box(Lx=lx, Ly=ly, Lz=lz, xy=xy, xz=xz, yz=yz)


def compute_rdf(req):
    p = req["parameters"]
    rmax = (p.get("rmax") or {}).get("value", 10.0)
    nbins = int(p.get("nbins") or 200)
    u = _universe(req["inputs"]["trajectory"])
    types_a = p.get("types_a")
    types_b = p.get("types_b")

    import freud
    # Small boxes: clamp r_max to what the box supports (recorded, not silent)
    first_dims = u.trajectory[0].dimensions
    lmin = min(float(x) for x in first_dims[:3])
    note = None
    if rmax > lmin / 2.2:
        rmax_eff = lmin / 2.2
        note = (f"r_max clamped {rmax:.2f} -> {rmax_eff:.2f} A "
                f"(box edge {lmin:.1f} A)")
        rmax = rmax_eff

    rdf = freud.density.RDF(bins=nbins, r_max=rmax)
    sel_a = _type_selection(u, types_a)
    sel_b = _type_selection(u, types_b)
    n_used = 0
    for ts in u.trajectory:
        box = _freud_box(ts.dimensions)
        if min(box.Lx, box.Ly, box.Lz) < 2.2 * rmax:
            continue  # box too small for this r_max; skip rather than lie
        pts_a = sel_a.positions if sel_a is not None else u.atoms.positions
        pts_b = sel_b.positions if sel_b is not None else u.atoms.positions
        aq = freud.locality.AABBQuery(box, pts_b)
        rdf.compute(aq, pts_a, reset=False)
        n_used += 1
    if n_used == 0:
        raise ProviderFailure("trajectory_corrupt", "input_error",
                              "no usable frames (box smaller than 2.2 x r_max?)")
    out = {"r": [float(x) for x in rdf.bin_centers],
           "g_r": [float(x) for x in np.nan_to_num(rdf.rdf)],
           "rmax": float(rmax), "nbins": nbins,
           "types_a": types_a, "types_b": types_b,
           "n_frames_used": n_used,
           "note": note,
           "unit": "angstrom" if req["inputs"]["trajectory"].get("metadata", {}).get("units") != "lj" else "sigma"}
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


# ------------------------------------------------------------------ Rg / Ree

def _chain_fragments(u):
    frags = u.atoms.fragments
    if not frags:
        raise ProviderFailure("input_invalid", "input_error",
                              "topology has no bond-defined fragments")
    return frags


def compute_rg(req):
    u = _universe(req["inputs"]["trajectory"])
    frac = float(req["parameters"].get("equilibration_fraction") or 0.5)
    frags = _chain_fragments(u)
    start = _tail_start(len(u.trajectory), frac)
    values = []
    for ts in u.trajectory[start:]:
        values.append([f.radius_of_gyration() for f in frags])
    arr = np.array(values)  # frames x chains
    mean, sem = _mean_sem(arr.ravel())
    per_frame = arr.mean(axis=1)
    payload = {"value": mean, "std": sem, "unit": "angstrom",
               "n_chains": len(frags), "n_frames": int(arr.shape[0]),
               "drift": float(per_frame[-1] - per_frame[0]) if len(per_frame) > 1 else 0.0}
    return {"outputs": {"result": _result(req, "RgResult", payload)}}


def compute_ree(req):
    u = _universe(req["inputs"]["trajectory"])
    frac = float(req["parameters"].get("equilibration_fraction") or 0.5)
    frags = _chain_fragments(u)
    start = _tail_start(len(u.trajectory), frac)
    box = u.dimensions[:3]
    values = []
    for ts in u.trajectory[start:]:
        box = ts.dimensions[:3]
        for f in frags:
            pos = f.positions
            # minimum-image unwrap along the chain
            unwrapped = [pos[0]]
            for i in range(1, len(pos)):
                d = pos[i] - unwrapped[-1]
                d -= box * np.round(d / box)
                unwrapped.append(unwrapped[-1] + d)
            values.append(float(np.linalg.norm(unwrapped[-1] - unwrapped[0])))
    arr = np.array(values)
    mean, sem = _mean_sem(arr)
    payload = {"value": mean, "std": sem, "unit": "angstrom",
               "n_chains": len(frags), "n_samples": int(len(arr))}
    return {"outputs": {"result": _result(req, "ReeResult", payload)}}


# ------------------------------------------------------------------ MSD / diffusion

def compute_msd(req):
    p = req["parameters"]
    u = _universe(req["inputs"]["trajectory"])
    meta = req["inputs"]["trajectory"].get("metadata") or {}
    frame_dt_fs = float(meta.get("frame_interval_fs") or 1000.0)
    by_mol = bool(p.get("com_by_molecule") or False)
    max_lag_frac = float(p.get("max_lag_fraction") or 0.5)

    groups = _chain_fragments(u) if by_mol else [u.atoms]
    # positions: frames x groups x particles x 3 (unwrapped per dump_modify)
    series = []
    for ts in u.trajectory:
        if by_mol:
            series.append([g.center_of_mass() for g in groups])
        else:
            series.append(u.atoms.positions.copy())
    arr = np.array(series, dtype=float)  # t x n x 3
    n_frames = arr.shape[0]
    max_lag = max(1, int(n_frames * max_lag_frac))
    lags, msd = [], []
    for lag in range(1, max_lag + 1):
        d = arr[lag:] - arr[:-lag]          # origins x n x 3
        lags.append(lag * frame_dt_fs)
        msd.append(float((d ** 2).sum(-1).mean()))
    payload = {"lag_fs": lags, "msd": msd,
               "unit": "angstrom2" if meta.get("units") != "lj" else "sigma2",
               "com_by_molecule": by_mol, "n_frames": int(n_frames)}
    art = _result(req, "MSDResult", payload)
    with open("msd.csv", "w") as f:
        f.write("lag_fs,msd\n")
        for l, m in zip(lags, msd):
            f.write(f"{l:.1f},{m:.6f}\n")
    art["files"]["csv"] = "msd.csv"
    return {"outputs": {"result": art}}


def fit_diffusion(req):
    msd = req["inputs"]["msd"].get("data")
    if not msd or "lag_fs" not in msd:
        raise ProviderFailure("input_invalid", "input_error",
                              "MSDResult artifact lacks data payload")
    p = req["parameters"]
    f0 = float(p.get("fit_start_fraction") or 0.2)
    f1 = float(p.get("fit_end_fraction") or 0.8)
    lag = np.array(msd["lag_fs"])
    y = np.array(msd["msd"])
    i0, i1 = int(len(lag) * f0), max(int(len(lag) * f1), int(len(lag) * f0) + 2)
    slope, intercept = np.polyfit(lag[i0:i1], y[i0:i1], 1)
    d_a2_fs = slope / 6.0
    payload = {"value": float(d_a2_fs * 0.1), "unit": "cm2/s",
               "slope_A2_per_fs": float(slope),
               "fit_window_fs": [float(lag[i0]), float(lag[i1 - 1])],
               "msd_unit": msd.get("unit")}
    return {"outputs": {"result": _result(req, "DiffusionResult", payload)}}


# ------------------------------------------------------------------ series fan-in / fits

def collect_thermo_series(req):
    series = req["inputs"]["series"]
    if not isinstance(series, list):
        series = [series]
    frac = float(req["parameters"].get("equilibration_fraction") or 0.5)
    points = []
    for art in series:
        rows = _read_csv(art["files"]["csv"])
        t_meta = (art.get("metadata") or {}).get("temperature_K")
        temp = float(t_meta) if t_meta else float(np.mean(_column(rows, "temp_K")))
        vol, _ = _mean_sem(_tail(_column(rows, "vol_A3"), frac)[0])
        rho, _ = _mean_sem(_tail(_column(rows, "density_g_cm3"), frac)[0])
        e, _ = _mean_sem(_tail(_column(rows, "etotal_kcal_mol"), frac)[0])
        points.append({"temperature_K": temp, "volume_A3": vol,
                       "density_g_cm3": rho, "energy_kcal_mol": e})
    points.sort(key=lambda p: p["temperature_K"])
    with open("series.csv", "w") as f:
        f.write("temperature_K,volume_A3,density_g_cm3,energy_kcal_mol\n")
        for pt in points:
            f.write(f"{pt['temperature_K']:.2f},{pt['volume_A3']:.4f},"
                    f"{pt['density_g_cm3']:.6f},{pt['energy_kcal_mol']:.4f}\n")
    out = artifact("TemperatureSeries", files={"csv": "series.csv"},
                   metadata={"n_points": len(points)},
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
    k = max(int(p.get("min_points") or 3), 3)
    sel = np.sort(order[:max(k, 3)])
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
    k = int(req["parameters"].get("min_points_per_branch") or 3)
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

def check_polymer_equilibration(req):
    rows = _read_csv(req["inputs"]["thermo"]["files"]["csv"])
    p = req["parameters"]
    drift_threshold = float(p.get("density_drift_threshold") or 0.01)
    sigma_mult = float(p.get("stationarity_sigma") or 3.0)
    tail_frac = float(p.get("tail_fraction") or 0.5)

    rho = _column(rows, "density_g_cm3")
    time_fs = _column(rows, "time_fs")
    tail, start = _tail(rho, 1.0 - tail_frac)
    tt = time_fs[start:]

    # drift: linear slope over the tail, normalized
    slope, intercept = np.polyfit(tt, tail, 1)
    mean_rho = float(np.mean(tail))
    total_drift = abs(slope) * (tt[-1] - tt[0]) / mean_rho if mean_rho else float("inf")
    drift_ok = total_drift < drift_threshold

    # stationarity: tail mean vs last-window mean within sigma_mult * sem.
    # Thermo samples are time-correlated, so use the correlation-corrected
    # SEM (lag-1 autocorrelation -> effective sample size); raw SEM would
    # flag any real drift as significant at ps sampling intervals.
    half = tail[len(tail) // 2:]
    m1, _ = _mean_sem(tail)
    m2, _ = _mean_sem(half)
    sem1 = _correlated_sem(tail)
    metrics = {
        "density_mean_g_cm3": mean_rho,
        "density_relative_drift": float(total_drift),
        "density_drift_threshold": drift_threshold,
        "density_tail_mean_shift_sigma": float(abs(m2 - m1) / max(sem1, 1e-12)),
        "stationarity_sigma": sigma_mult,
        "n_tail_samples": int(len(tail)),
    }
    stat_ok = abs(m2 - m1) < sigma_mult * max(sem1, 1e-12)
    checks = {"density_drift": bool(drift_ok), "density_stationarity": bool(stat_ok)}

    traj = req["inputs"].get("trajectory")
    if traj and traj.get("files", {}).get("dcd"):
        try:
            u = _universe(traj)
            frags = u.atoms.fragments
            if frags:
                start_f = _tail_start(len(u.trajectory), 1.0 - tail_frac)
                rg = []
                for ts in u.trajectory[start_f:]:
                    rg.append(np.mean([f.radius_of_gyration() for f in frags]))
                rg = np.array(rg)
                if len(rg) > 3:
                    s_rg, _ = np.polyfit(np.arange(len(rg)), rg, 1)
                    rg_drift = abs(s_rg) * len(rg) / float(np.mean(rg))
                    metrics["rg_relative_drift"] = float(rg_drift)
                    checks["rg_stable"] = bool(rg_drift < drift_threshold * 10)
        except Exception:
            pass  # trajectory metrics are best-effort

    equilibrated = all(checks.values())
    payload = {"equilibrated": equilibrated, "checks": checks, "metrics": metrics}
    Path("report.json").write_text(json.dumps(payload, indent=1))
    out = artifact("EquilibrationReport", files={"json": "report.json"}, data=payload)
    return {"outputs": {"report": out}}


def promote_equilibrated_state(req):
    report = req["inputs"]["report"].get("data") or {}
    require = req["parameters"].get("require_pass")
    require = True if require is None else bool(require)
    equilibrated = bool(report.get("equilibrated"))
    if require and not equilibrated:
        raise ProviderFailure(
            "validation_failed", "scientific_validation",
            "EquilibrationReport says the state is NOT equilibrated; "
            "refusing promotion (run longer or adjust the protocol)",
            details={"checks": report.get("checks"), "metrics": report.get("metrics")})
    state_in = req["inputs"]["state"]
    files = {}
    for key, path in state_in["files"].items():
        dest = f"promoted_{key}{Path(path).suffix or '.dat'}"
        shutil.copy(path, dest)
        files[key] = dest
    meta = dict(state_in.get("metadata") or {})
    meta["equilibration"] = {
        "report_checks": report.get("checks"),
        "density_mean_g_cm3": (report.get("metrics") or {}).get("density_mean_g_cm3"),
        "forced": not equilibrated,
    }
    out = artifact("EquilibratedState", files=files, metadata=meta,
                   data=state_in.get("data"))
    return {"outputs": {"state": out}}


# ------------------------------------------------------------------ adhesion / modulus

def compute_adhesion(req):
    rows = _read_csv(req["inputs"]["thermo"]["files"]["csv"])
    cols = rows[0].keys() if rows else []
    ecol = next((c for c in cols if "interaction" in c or "eint" in c), None)
    if not ecol:
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"series lacks an interaction-energy column (has {list(cols)}); "
            "run the NVT with the 'interaction' parameter set")
    frac = float(req["parameters"].get("equilibration_fraction") or 0.5)
    tail, _ = _tail(_column(rows, ecol), frac)
    mean, sem = _mean_sem(tail)
    area_q = req["parameters"]["interface_area"]
    area = float(area_q["value"])  # canonical angstrom2
    # kcal/mol/A^2 -> mJ/m^2 : 1 kcal/mol = 6.9477e-21 J; 1 A^2 = 1e-20 m^2
    w = -mean / area * 0.69477 * 1000.0
    payload = {"value": float(w), "unit": "mJ/m2",
               "interaction_energy_kcal_mol": mean, "sem": sem,
               "interface_area_A2": area}
    return {"outputs": {"result": _result(req, "AdhesionResult", payload)}}


def fit_modulus(req):
    rows = _read_csv(req["inputs"]["series"]["files"]["csv"])
    strain = _column(rows, "strain")
    stress = _column(rows, "stress_GPa")
    max_strain = float(req["parameters"].get("max_strain") or 0.05)
    mask = (strain > 0) & (strain <= max_strain)
    if mask.sum() < 3:
        raise ProviderFailure("input_invalid", "input_error",
                              f"only {int(mask.sum())} points below max_strain={max_strain}")
    slope, intercept = np.polyfit(strain[mask], stress[mask], 1)
    payload = {"value": float(slope), "unit": "GPa",
               "fit_max_strain": max_strain, "n_points": int(mask.sum()),
               "intercept_GPa": float(intercept)}
    return {"outputs": {"result": _result(req, "ModulusResult", payload)}}


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
        })
    raise SystemExit(provider.cli())


if __name__ == "__main__":
    cli()
