"""Regression tests for the 2026-10-07 code review (correct behavior).

No simulation engine is needed: LAMMPS decks are generated and inspected,
`_run_lammps` is replaced by a fake that writes known outputs, and
trajectory-based analyses run on tiny in-memory MDAnalysis universes.
"""

import csv
import json
import math
import os
import sys
from pathlib import Path
from unittest.mock import patch

import numpy as np
import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import m3flow_analysis.main as analysis  # noqa: E402
import m3flow_lammps.main as lammps  # noqa: E402
from m3flow_provider import Provider, ProviderFailure  # noqa: E402

mda = pytest.importorskip("MDAnalysis")


@pytest.fixture(autouse=True)
def _isolated_cwd(tmp_path, monkeypatch):
    """Tasks write result files into the cwd (the provider workdir)."""
    monkeypatch.chdir(tmp_path)


@pytest.fixture
def wd(tmp_path, monkeypatch):
    src = tmp_path / "input"
    src.mkdir()
    (src / "state.data").write_text("placeholder\n")
    (src / "state.init").write_text("units real\natom_style full\n")
    work = tmp_path / "work"
    work.mkdir()
    monkeypatch.chdir(work)
    return tmp_path


def _state(wd, units="real", init="units real\natom_style full\n"):
    (wd / "input" / "state.init").write_text(init)
    return {"id": "art_state_A", "content_hash": "hash_A", "producer": "tr_A",
            "files": {"data": str(wd / "input" / "state.data"),
                      "init": str(wd / "input" / "state.init")},
            "metadata": {"units": units, "atom_style": "full"}}


def _params(**kw):
    p = {"temperature": {"value": 300, "unit": "K"},
         "pressure": {"value": 1, "unit": "bar"},
         "duration": {"value": 1000, "unit": "fs"},
         "timestep": {"value": 1, "unit": "fs"}}
    p.update(kw)
    return p


def _req(wd, units="real", **params):
    return {"inputs": {"state": _state(wd, units)}, "parameters": _params(**params),
            "workdir": str(wd / "work")}


# ------------------------------------------------------------------ LAMMPS decks

EXPECTED_FIXES = {
    ("nose_hoover", "nose_hoover"): ["npt"],
    ("langevin", "nose_hoover"): ["nph", "langevin"],
    ("berendsen", "nose_hoover"): ["nph", "temp/berendsen"],
    ("nose_hoover", "berendsen"): ["nvt", "press/berendsen"],
    ("langevin", "berendsen"): ["nve", "langevin", "press/berendsen"],
    ("berendsen", "berendsen"): ["nve", "temp/berendsen", "press/berendsen"],
}


@pytest.mark.parametrize("combo", sorted(EXPECTED_FIXES))
def test_npt_combinations_map_to_explicit_fix_sets(wd, combo):
    tstat, bstat = combo
    ctx = lammps.Ctx(_req(wd, thermostat=tstat, barostat=bstat))
    lines, _ = lammps.deck_run(ctx, "npt")
    fixes = [ln.split() for ln in lines if ln.startswith("fix ")]
    styles = [f[3] for f in fixes if f[1] != "m3mom"]
    assert styles == EXPECTED_FIXES[combo]
    created = {f[1] for f in fixes}
    removed = {ln.split()[1] for ln in lines if ln.startswith("unfix ")}
    assert removed == created
    # every NPT variant controls pressure
    assert any(s in ("npt", "nph", "press/berendsen") for s in styles)


def test_berendsen_barostat_gets_a_physical_modulus(wd):
    ctx = lammps.Ctx(_req(wd, barostat="berendsen"))
    lines, _ = lammps.deck_run(ctx, "npt")
    fix = next(ln for ln in lines if "press/berendsen" in ln).split()
    modulus_atm = float(fix[fix.index("modulus") + 1])
    assert math.isclose(modulus_atm, 20000.0 / 1.01325, rel_tol=1e-9)
    ctx = lammps.Ctx(_req(wd, barostat="berendsen",
                          berendsen_modulus={"value": 1, "unit": "GPa"}))
    lines, _ = lammps.deck_run(ctx, "npt")
    fix = next(ln for ln in lines if "press/berendsen" in ln).split()
    assert math.isclose(float(fix[fix.index("modulus") + 1]), 10000.0 / 1.01325)


def test_berendsen_barostat_rejects_triclinic_style(wd):
    ctx = lammps.Ctx(_req(wd, barostat="berendsen", pressure_style="tri"))
    with pytest.raises(ProviderFailure):
        lammps.deck_run(ctx, "npt")


def test_pressure_units_reach_the_deck_in_atm(wd):
    ctx = lammps.Ctx(_req(wd, pressure={"value": 100, "unit": "kPa"}))
    lines, _ = lammps.deck_run(ctx, "npt")
    npt = next(ln for ln in lines if " npt " in ln).split()
    p0 = float(npt[npt.index("iso") + 1])
    assert math.isclose(p0, 1.0 / 1.01325, rel_tol=1e-9)  # 1 bar in atm


def test_unknown_unit_style_is_rejected(wd):
    with pytest.raises(ProviderFailure):
        lammps.Ctx(_req(wd, units="metal"))


def test_actual_sampling_interval_is_recorded(wd):
    req = _req(wd, timestep={"value": 2, "unit": "fs"},
               sampling_interval={"value": 3, "unit": "fs"})
    ctx = lammps.Ctx(req)
    lines, _ = lammps.deck_run(ctx, "nvt")
    assert "dump m3d all dcd 2 traj.dcd" in lines
    out = lammps._run_outputs(ctx, True, None, {"ensemble": "nvt"})
    meta = out["trajectory"]["metadata"]
    assert meta["frame_interval_fs"] == 4.0 and meta["dump_stride"] == 2
    assert any("3 fs" in w for w in ctx.warnings)


def test_stress_conversion_atm_to_gpa(wd):
    def fake_run(ctx, deck, has_trajectory):
        atm_for_one_gpa = 1e9 / 101325
        (ctx.workdir / "stress_strain.csv").write_text(f"0.01 {atm_for_one_gpa}\n")
        return "Total wall time: 0:00:00\n"

    with patch.object(lammps, "_run_lammps", fake_run):
        res = lammps.run_deform(_req(wd))
    with open(wd / "work" / "stress_strain_series.csv") as f:
        row = next(csv.DictReader(f))
    assert math.isclose(float(row["stress_GPa"]), 1.0, rel_tol=1e-6)
    assert res["outputs"]["series"]["metadata"]["stress_unit"] == "GPa"


def test_lj_stress_stays_reduced(wd):
    def fake_run(ctx, deck, has_trajectory):
        (ctx.workdir / "stress_strain.csv").write_text("0.01 2.5\n")
        return "Total wall time: 0:00:00\n"

    with patch.object(lammps, "_run_lammps", fake_run):
        res = lammps.run_deform(_req(wd, units="lj"))
    meta = res["outputs"]["series"]["metadata"]
    assert meta["stress_unit"] == "lj" and meta["stress_column"] == "stress_lj"
    with open(wd / "work" / "stress_strain_series.csv") as f:
        assert float(next(csv.DictReader(f))["stress_lj"]) == 2.5


def test_deform_measures_strain_from_the_box(wd):
    ctx = lammps.Ctx(_req(wd))
    lines, _ = lammps.deck_deform(ctx)
    assert "variable m3strain equal (lz-v_m3L0)/v_m3L0" in lines
    assert any("remap x" in ln for ln in lines)


def test_lj_thermo_columns_are_reduced(wd):
    ctx = lammps.Ctx(_req(wd, units="lj"))
    lammps.deck_run(ctx, "nvt")
    thermo = {"columns": ["Step", "Temp", "Density", "TotEng"],
              "rows": [[0, 1.0, 0.85, -3.0], [100, 1.0, 0.85, -3.0]]}
    art = lammps._thermo_artifact(ctx, thermo, "nvt")
    cols = art["data"]["columns"]
    assert cols == ["time_tau", "temp_lj", "density_lj", "etotal_lj"]
    assert art["metadata"]["time_unit"] == "tau"


def test_interaction_adds_kspace_component_when_input_has_kspace(wd):
    req = _req(wd, interaction={"group_a": "molecule 1", "group_b": "molecule 2"})
    _state(wd, init="units real\natom_style full\nkspace_style pppm 1e-5\n")
    ctx = lammps.Ctx(req)
    lines, _ = lammps.deck_run(ctx, "nvt")
    assert "compute m3_eintk m3_ga group/group m3_gb pair no kspace yes" in lines
    assert any("intersect m3_ga m3_gb" in ln for ln in lines)
    thermo = {"columns": ["Step", "c_m3_eint", "c_m3_eintk"],
              "rows": [[0, -10.0, -2.0]]}
    art = lammps._thermo_artifact(ctx, thermo, "nvt")
    text = (wd / "work" / "thermo.csv").read_text().splitlines()
    assert text[0].split(",")[-1] == "e_interaction_kcal_mol"
    assert float(text[1].split(",")[-1]) == -12.0
    assert art["metadata"]["interaction"]["kspace_included"] is True


def test_group_overlap_marker_fails_the_task():
    log = "M3FLOW_CHECK_FAILED: interaction groups overlap in 12 atoms\n"
    with pytest.raises(ProviderFailure) as e:
        lammps._classify_failure(3, log)
    assert e.value.error_type == "input_invalid"


def test_engine_probe_uses_configured_binary(tmp_path):
    exe = tmp_path / "lmp_custom"
    exe.write_text("#!/bin/sh\necho 'Large-scale Atomic/Molecular Massively "
                   "Parallel Simulator - 1 Jan 2030'\n")
    exe.chmod(0o755)
    desc = lammps._engine({"config": {"executable": str(exe)}})
    assert desc["version"] == "1 Jan 2030"
    assert desc["executable"] == str(exe.resolve())


# ------------------------------------------------------------------ statistics

def _thermo(path, header, rows):
    with open(path, "w") as f:
        f.write(",".join(header) + "\n")
        for r in rows:
            f.write(",".join(str(x) for x in r) + "\n")
    return {"id": "art_thermo", "content_hash": "hash_thermo", "producer": "tr_A",
            "files": {"csv": str(path)}, "metadata": {}}


def test_equilibration_fraction_discards_the_head(wd):
    th = _thermo(wd / "t.csv", ["density_g_cm3"], [[x] for x in range(1, 11)])
    d = analysis.compute_density({"inputs": {"thermo": th},
                                  "parameters": {"equilibration_fraction": 0.2}})
    data = d["outputs"]["result"]["data"]
    assert data["n_skipped"] == 2 and data["n_samples"] == 8
    assert data["value"] == 6.5


def test_zero_fraction_is_honored(wd):
    th = _thermo(wd / "t.csv", ["density_g_cm3"], [[x] for x in range(1, 11)])
    d = analysis.compute_density({"inputs": {"thermo": th},
                                  "parameters": {"equilibration_fraction": 0}})
    assert d["outputs"]["result"]["data"]["n_skipped"] == 0


def test_out_of_range_fraction_is_rejected(wd):
    th = _thermo(wd / "t.csv", ["density_g_cm3"], [[1], [2]])
    with pytest.raises(ProviderFailure):
        analysis.compute_density({"inputs": {"thermo": th},
                                  "parameters": {"equilibration_fraction": 1.2}})


def test_single_sample_reports_null_uncertainty_not_nan(wd):
    th = _thermo(wd / "t.csv", ["density_g_cm3"], [[1], [2]])
    d = analysis.compute_density({"inputs": {"thermo": th}, "parameters": {}})
    data = d["outputs"]["result"]["data"]
    assert data["sem"] is None and data["sem_status"] == "insufficient_data"
    json.dumps(data, allow_nan=False)  # strict JSON


def test_execute_rejects_non_finite_payloads(tmp_path):
    def bad(req):
        return {"outputs": {"r": {"type": "Result", "files": {}, "data": {"x": float("nan")}}}}

    prov = Provider("t", "0", lambda: {"name": "x", "version": "1"}, {"bad": bad})
    req = tmp_path / "req.json"
    req.write_text(json.dumps({"protocol": "m3flow-provider/1", "task": {"name": "bad"},
                               "workdir": str(tmp_path / "w"), "inputs": {}, "parameters": {}}))
    cwd = os.getcwd()
    try:
        with pytest.raises(ProviderFailure) as e:
            prov.execute(str(req))
    finally:
        os.chdir(cwd)
    assert e.value.error_type == "non_finite_value"


def test_blocking_sem_white_noise_and_correlated():
    rng = np.random.default_rng(0)
    x = rng.normal(size=4096)
    b = analysis.blocking_sem(x)
    assert b["converged"]
    assert 0.7 < b["sem"] / (x.std(ddof=1) / 64) < 1.4
    rho = 0.9
    y = np.empty(1 << 15)
    y[0] = 0.0
    eps = rng.normal(size=len(y))
    for i in range(1, len(y)):
        y[i] = rho * y[i - 1] + eps[i]
    iid = y.std(ddof=1) / math.sqrt(len(y))
    ratio = analysis.blocking_sem(y)["sem"] / iid
    assert 3.0 < ratio < 6.0  # theory: sqrt((1+rho)/(1-rho)) = 4.36


# ------------------------------------------------------------------ equilibration gate

def _eq_series(wd, energy):
    rows = [[t, 1.0, energy(t), 1000.0] for t in range(100)]
    return _thermo(wd / "eq.csv",
                   ["time_fs", "density_g_cm3", "etotal_kcal_mol", "vol_A3"], rows)


def test_energy_drift_fails_the_gate(wd):
    th = _eq_series(wd, lambda t: t * 100.0)
    rep = analysis.check_polymer_equilibration(
        {"inputs": {"thermo": th}, "parameters": {}})["outputs"]["report"]["data"]
    assert rep["checks"]["energy_trend"] == "failed"
    assert rep["equilibrated"] is False


def test_stationary_noise_passes(wd):
    rng = np.random.default_rng(1)
    noise = rng.normal(size=100)
    th = _eq_series(wd, lambda t: -500.0 + noise[t])
    rep = analysis.check_polymer_equilibration(
        {"inputs": {"thermo": th}, "parameters": {}})["outputs"]["report"]["data"]
    assert rep["checks"]["energy_trend"] == "passed", rep["check_details"]
    assert rep["equilibrated"] is True
    assert rep["evidence"]["thermo"]["content_hash"] == "hash_thermo"


def test_unreadable_trajectory_is_not_a_pass(wd):
    th = _eq_series(wd, lambda t: -500.0)
    req = {"inputs": {"thermo": th,
                      "trajectory": {"files": {"dcd": "/missing", "topology": "/missing"}}},
           "parameters": {}}
    with patch.object(analysis, "_universe", side_effect=ValueError("corrupt")):
        rep = analysis.check_polymer_equilibration(req)["outputs"]["report"]["data"]
    assert rep["checks"]["rg_stable"] == "error"
    assert rep["equilibrated"] is False


def _passing_report(subject=None, producer="tr_A"):
    return {"id": "art_report", "data": {
        "equilibrated": True, "criteria_version": analysis.CRITERIA_VERSION,
        "checks": {"density_drift": "passed"},
        "evidence": {"thermo": {"producer": producer}}, "subject": subject}}


def test_promotion_requires_report_bound_to_state(wd):
    state = _state(wd)
    other = _passing_report(subject={"state_content_hash": "hash_B"})
    with pytest.raises(ProviderFailure) as e:
        analysis.promote_equilibrated_state(
            {"inputs": {"state": state, "report": other}, "parameters": {}})
    assert e.value.error_type == "report_not_bound"
    with pytest.raises(ProviderFailure):
        analysis.promote_equilibrated_state(
            {"inputs": {"state": state, "report": _passing_report(producer="tr_B")},
             "parameters": {}})


def test_promotion_accepts_bound_report(wd):
    state = _state(wd)
    for report in (_passing_report(), _passing_report(subject={"state_content_hash": "hash_A"})):
        out = analysis.promote_equilibrated_state(
            {"inputs": {"state": state, "report": report}, "parameters": {}})
        st = out["outputs"]["state"]
        assert st["type"] == "EquilibratedState"
        assert st["metadata"]["equilibration"]["status"] == "certified"


def test_forced_promotion_is_refused(wd):
    report = _passing_report()
    report["data"]["equilibrated"] = False
    with pytest.raises(ProviderFailure):
        analysis.promote_equilibrated_state(
            {"inputs": {"state": _state(wd), "report": report},
             "parameters": {"require_pass": False}})
    with pytest.raises(ProviderFailure) as e:
        analysis.promote_equilibrated_state(
            {"inputs": {"state": _state(wd), "report": report}, "parameters": {}})
    assert e.value.error_type == "validation_failed"


def test_legacy_report_without_binding_is_refused(wd):
    legacy = {"id": "r", "data": {"equilibrated": True, "checks": {"density_drift": True}}}
    with pytest.raises(ProviderFailure) as e:
        analysis.promote_equilibrated_state(
            {"inputs": {"state": _state(wd), "report": legacy}, "parameters": {}})
    assert e.value.error_type == "report_not_bound"


# ------------------------------------------------------------------ MSD / diffusion

def test_msd_fft_matches_direct_average():
    rng = np.random.default_rng(2)
    x = np.cumsum(rng.normal(size=(50, 7, 3)), axis=0)
    fft = analysis.msd_fft(x)
    for lag in range(1, 50):
        direct = ((x[lag:] - x[:-lag]) ** 2).sum(-1).mean()
        assert math.isclose(fft[lag], direct, rel_tol=1e-9, abs_tol=1e-9)


def _moving_universe(n_frames=12, dims=(50.0, 50.0, 50.0, 90.0, 90.0, 90.0)):
    u = mda.Universe.empty(1, trajectory=True)
    coords = np.array([[[i, 0.0, 0.0]] for i in range(n_frames)], dtype=np.float32)
    u.load_new(coords, dimensions=np.array(dims))
    return u


def test_msd_time_axis_comes_from_metadata(wd):
    def msd_at(meta):
        with patch.object(analysis, "_universe", return_value=_moving_universe()):
            return analysis.compute_msd({"inputs": {"trajectory": {
                "files": {}, "metadata": meta}}, "parameters": {}})["outputs"]["result"]["data"]
    a = msd_at({"frame_interval_fs": 100})
    b = msd_at({"frame_interval_fs": 1000})
    assert b["lag"][0] == 10 * a["lag"][0] and a["msd"] == b["msd"]
    assert math.isclose(a["msd"][0], 1.0, rel_tol=1e-6)
    with pytest.raises(ProviderFailure):
        msd_at({})


def test_reduced_msd_gives_reduced_diffusion():
    lag = list(range(1, 21))
    msd = {"lag": lag, "time_unit": "tau", "msd": [6 * x for x in lag], "unit": "sigma2"}
    d = analysis.fit_diffusion({"inputs": {"msd": {"data": msd}}, "parameters": {}})
    data = d["outputs"]["result"]["data"]
    assert data["unit"] == "sigma2/tau" and math.isclose(data["value"], 1.0)
    mapped = analysis.fit_diffusion({"inputs": {"msd": {"data": msd}}, "parameters": {
        "sigma": {"value": 3.0, "unit": "angstrom"}, "tau": {"value": 2000.0, "unit": "fs"}}})
    data = mapped["outputs"]["result"]["data"]
    assert data["unit"] == "cm2/s"
    assert math.isclose(data["value"], 1.0 * 9.0 / 2000.0 * 0.1)


def test_physical_diffusion_units():
    lag = list(range(1, 21))
    msd = {"lag_fs": lag, "msd": [6 * x for x in lag], "unit": "angstrom2"}
    data = analysis.fit_diffusion({"inputs": {"msd": {"data": msd}},
                                   "parameters": {}})["outputs"]["result"]["data"]
    assert data["unit"] == "cm2/s" and math.isclose(data["value"], 0.1)


# ------------------------------------------------------------------ geometry / topology

def test_triclinic_box_conversion():
    pytest.importorskip("freud")
    box = analysis._freud_box([10, 10, 10, 90, 90, 60])
    assert math.isclose(box.Lx, 10, rel_tol=1e-6)
    assert math.isclose(box.Ly, 8.660254, rel_tol=1e-6)
    assert math.isclose(box.xy, 0.577350, rel_tol=1e-5)
    assert abs(box.xz) < 1e-6 and abs(box.yz) < 1e-6
    widths = analysis._plane_widths([10, 10, 10, 90, 90, 60])
    assert np.allclose(widths, [8.660254, 8.660254, 10.0], rtol=1e-6)


def test_rdf_of_ideal_gas_in_triclinic_cell_is_flat(wd):
    """Uniform points in a tilted cell: g(r) must be ~1 at every r (no
    self-pair spike at r = 0, correct cell volume and periodicity)."""
    pytest.importorskip("freud")
    rng = np.random.default_rng(3)
    dims = np.array([20.0, 20.0, 17.32, 90.0, 90.0, 60.0])
    m = analysis._box_matrix(dims)
    frames = np.array([rng.random((2000, 3)) @ m for _ in range(5)], dtype=np.float32)
    u = mda.Universe.empty(2000, trajectory=True)
    u.add_TopologyAttr("types", ["1"] * 2000)
    u.load_new(frames, dimensions=dims)
    with patch.object(analysis, "_universe", return_value=u):
        out = analysis.compute_rdf({"inputs": {"trajectory": {"metadata": {}}},
                                    "parameters": {"rmax": {"value": 6.0, "unit": "angstrom"},
                                                   "nbins": 30}})
    data = out["outputs"]["result"]["data"]
    g = np.array(data["g_r"])
    r = np.array(data["r"])
    assert data["note"] is None
    assert abs(g[r > 2.0].mean() - 1.0) < 0.03, g
    assert g[0] < 3.0  # i == i pairs would put ~100 here


def _chain_universe(positions, bonds, dims=(10.0, 10.0, 10.0, 90.0, 90.0, 90.0)):
    n = len(positions)
    u = mda.Universe.empty(n, n_residues=1, atom_resindex=[0] * n, trajectory=True)
    u.add_TopologyAttr("masses", [1.0] * n)
    u.add_TopologyAttr("names", [f"C{i}" for i in range(n)])
    u.add_TopologyAttr("bonds", bonds)
    u.load_new(np.array([positions] * 4, dtype=np.float32), dimensions=np.array(dims))
    return u


def test_ree_follows_bond_graph_not_atom_order(wd):
    # bonded chain 0 -- 2 -- 1 at x = 0, 1, 2 (atom 1 sits at x = 2)
    u = _chain_universe([[0, 0, 0], [2, 0, 0], [1, 0, 0]], [(0, 2), (2, 1)])
    with patch.object(analysis, "_universe", return_value=u):
        ree = analysis.compute_ree({"inputs": {"trajectory": {}},
                                    "parameters": {}})["outputs"]["result"]["data"]
    assert math.isclose(ree["value"], 2.0, rel_tol=1e-6)


def test_ree_across_periodic_boundary(wd):
    # chain 0-1-2 straddling the boundary of a 10 A box, wrapped coordinates
    u = _chain_universe([[9.5, 0, 0], [0.5, 0, 0], [1.5, 0, 0]], [(0, 1), (1, 2)])
    with patch.object(analysis, "_universe", return_value=u):
        ree = analysis.compute_ree({"inputs": {"trajectory": {}},
                                    "parameters": {}})["outputs"]["result"]["data"]
    assert math.isclose(ree["value"], 2.0, rel_tol=1e-6)


def test_branched_chain_needs_explicit_ends(wd):
    u = _chain_universe([[0, 0, 0], [1, 0, 0], [2, 0, 0], [1, 1, 0]],
                        [(0, 1), (1, 2), (1, 3)])
    with patch.object(analysis, "_universe", return_value=u):
        with pytest.raises(ProviderFailure):
            analysis.compute_ree({"inputs": {"trajectory": {}}, "parameters": {}})
        ree = analysis.compute_ree({"inputs": {"trajectory": {}}, "parameters": {
            "end_selection": "index 0 or index 2"}})["outputs"]["result"]["data"]
    assert math.isclose(ree["value"], 2.0, rel_tol=1e-6)


# ------------------------------------------------------------------ adhesion / modulus

def test_adhesion_reports_components_and_area(wd):
    rows = [[t, -100.0, -20.0, -120.0, 10.0, 20.0, 50.0] for t in range(10)]
    th = _thermo(wd / "a.csv", ["time_fs", "e_interaction_pair_kcal_mol",
                                "e_interaction_kspace_kcal_mol", "e_interaction_kcal_mol",
                                "lx_A", "ly_A", "lz_A"], rows)
    th["metadata"] = {"interaction": {"kspace_included": True,
                                      "observable": "group/group interaction energy (pair + kspace)"}}
    out = analysis.compute_adhesion({"inputs": {"thermo": th},
                                     "parameters": {"n_interfaces": 2}})
    data = out["outputs"]["result"]["data"]
    assert data["interface_area"] == 200.0 and data["n_interfaces"] == 2
    assert math.isclose(data["value"], 120.0 / 400.0 * analysis.KCAL_MOL_A2_TO_MJ_M2)
    assert data["interaction_energy_kspace"] == -20.0
    assert data["observable"] == "interaction_energy_per_area"
    assert data["kspace_included"] is True


def test_modulus_uses_recorded_stress_unit(wd):
    rows = [[i * 0.01, i * 0.01 * 3.0] for i in range(1, 6)]
    path = wd / "ss.csv"
    with open(path, "w") as f:
        f.write("strain,stress_GPa\n")
        for r in rows:
            f.write(f"{r[0]},{r[1]}\n")
    res = analysis.fit_modulus({"inputs": {"series": {
        "files": {"csv": str(path)}, "metadata": {"stress_column": "stress_GPa"}}},
        "parameters": {}})["outputs"]["result"]["data"]
    assert math.isclose(res["value"], 3.0, rel_tol=1e-9) and res["unit"] == "GPa"


# ------------------------------------------------------------------ SystemSpec geometry

def test_system_spec_box_and_density_units():
    import m3flow_autopoly.main as autopoly
    env = {"box": {"x": "4 nm", "y": "40 angstrom", "z": {"value": 5, "unit": "nm"}},
           "target_density": "900 kg/m3", "gap": "0.3 nm"}
    density, size, dims = autopoly._env_geometry(env, {})
    assert dims == (40.0, 40.0, 50.0) and size is None
    assert math.isclose(density, 0.9)
    # the canonical task parameter wins over the spec
    density, _, _ = autopoly._env_geometry(env, {"target_density": {"value": 1.1, "unit": "g/cm3"}})
    assert density == 1.1
    _, size, dims = autopoly._env_geometry({"box": "3 nm"}, {})
    assert size == 30.0 and dims is None
    with pytest.raises(ProviderFailure):
        autopoly._env_geometry({"box": {"x": "4 furlong", "y": 1, "z": 1}}, {})
    with pytest.raises(ProviderFailure):
        autopoly._env_geometry({"box": {"x": "4 nm"}}, {})
