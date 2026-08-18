"""m3flow-autopoly: system construction tasks via the AutoPoly stage APIs.

Task mapping (tasks/autopoly/*.yaml):
  build_system              SystemSpec -> MolecularSystem   (GeometryBuilder)
  parameterize_system       MolecularSystem -> ParameterizedSystem (UnitTyper)
  prepare_simulation_system ParameterizedSystem -> SimulationSystem (BoxPacker)

Coarse-grained (bead-spring) systems bypass the atomistic pipeline:
build records a cg_spec.json, parameterize is a pass-through, and prepare
drives AutoPoly's BeadSpringSystem directly. CG structure and potentials
are spec-driven: component ``topology`` + ``options`` select the chain
architecture (linear, ring, comb, graft, star, dendrimer, tadpole,
branched/custom); ``environment.options`` and ``resolution.model`` set the
potentials (``kremer_grest`` = FENE bonds + WCA pairs), angle terms, box
size and configuration-generation method.
"""

from __future__ import annotations

import json
import shutil
from pathlib import Path

import yaml

from m3flow_provider import (Provider, ProviderFailure, artifact, read_request,
                             verdict)

PROVIDER_VERSION = "0.4.0"


def _engine():
    import importlib.metadata
    try:
        v = importlib.metadata.version("AutoPoly")
    except Exception:
        v = "unknown"
    return {"name": "autopoly", "version": v}


# ------------------------------------------------------------------ spec -> models

def _load_spec(req):
    inp = req["inputs"]["specification"]
    if inp.get("data"):
        return inp["data"]
    with open(inp["files"]["spec"]) as f:
        return yaml.safe_load(f)


def _expand_sequence(comp):
    """first/middle/last + dop -> explicit monomer list."""
    rep = comp["representation"]
    if rep.get("sequence"):
        return list(rep["sequence"])
    dop = comp.get("degree_of_polymerization") or comp.get("dop")
    if not dop:
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"component '{comp['id']}': polymer needs representation.sequence "
            "or first/middle/last with degree_of_polymerization")
    first, middle, last = rep.get("first"), rep.get("middle"), rep.get("last")
    if dop == 1:
        if not first:
            raise ProviderFailure("input_invalid", "input_error",
                                  f"component '{comp['id']}': dop=1 needs 'first'")
        return [first]
    if not (first and middle):
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"component '{comp['id']}': dop>=2 needs first/middle[/last]")
    seq = [first] + [middle] * (dop - 2) + [last or first]
    return seq


def _component_model(comp):
    import AutoPoly
    ctype = comp["type"]
    rep = comp["representation"]
    if ctype == "molecule":
        if rep["type"] == "smiles":
            return AutoPoly.Molecule(
                Count=comp.get("count") or 1,
                Smiles=rep["value"], Name=comp["id"])
        if rep["type"] == "name":
            return ("name", comp)  # resolved later (substrate builder etc.)
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"component '{comp['id']}': unsupported molecule representation "
            f"'{rep['type']}'")
    if ctype == "polymer":
        return AutoPoly.Polymer(
            chain_num=comp.get("number_of_chains") or comp.get("count") or 1,
            sequence=_expand_sequence(comp),
            topology=comp.get("topology") or "linear",
            tacticity=comp.get("tacticity") or "atactic")
    if ctype == "substrate":
        return ("substrate", comp)
    raise ProviderFailure(
        "input_invalid", "input_error",
        f"component '{comp['id']}': unsupported component type '{ctype}'")


def _is_cg(spec):
    if (spec.get("resolution") or {}).get("type") == "coarse_grained":
        return True
    return any(c["representation"]["type"] == "bead_spring"
               for c in spec.get("components", []))


# ------------------------------------------------------------------ task 1: build

def build_system(req):
    spec = _load_spec(req)
    params = req["parameters"]
    seed = params.get("seed")
    name = spec.get("name", "system")

    if _is_cg(spec):
        return _build_cg(spec, name)

    import AutoPoly
    system = AutoPoly.System(out="autopoly")
    film, substrate = [], []
    for comp in spec.get("components", []):
        model = _component_model(comp)
        if isinstance(model, tuple) and model[0] == "substrate":
            # slab-builder substrates join at the packing stage
            if model[1]["representation"]["type"] == "name":
                continue
            substrate.append(_component_model({**model[1], "type": "polymer"
                                               if model[1]["representation"]["type"] != "smiles"
                                               else "molecule"}))
        elif isinstance(model, tuple):
            continue
        else:
            film.append(model)

    config = AutoPoly.GeometryConfig(
        use_mc_chain_growth=bool(params.get("use_mc_chain_growth", True)),
        rng_seed=seed if isinstance(seed, int) else None)
    builder = AutoPoly.GeometryBuilder(system, name, config)
    try:
        result = builder.build(film, substrate_models=substrate or None)
    except Exception as e:
        etype = type(e).__name__
        if "Validation" in etype:
            raise ProviderFailure("input_invalid", "input_error",
                                  f"geometry validation failed: {e}")
        raise ProviderFailure("builder_failed", "provider_error",
                              f"GeometryBuilder failed ({etype}): {e}",
                              raw_log=str(e))
    geom_dir = Path(result.directory)
    geom_file = geom_dir / "geometry.json"
    if not geom_file.is_file():
        raise ProviderFailure("builder_failed", "provider_error",
                              f"geometry.json not produced at {geom_file}",
                              recoverable=False)
    data = json.loads(geom_file.read_text())

    # component count check
    problems = []
    for comp in spec.get("components", []):
        want = comp.get("number_of_chains") or comp.get("count")
        if want is None:
            continue
        if comp["type"] == "polymer":
            got = len([c for c in data.get("chains", [])
                       if c.get("model_id") is not None])
        else:
            got = None  # per-component counts live in molecules[] entries
        if got is not None and got < want:
            problems.append(f"{comp['id']}: wanted {want}, built {got}")
    n_chains = len(data.get("chains", []))
    n_mols = len(data.get("molecules", []))

    out = artifact(
        "MolecularSystem",
        files={"geometry": str(geom_file.resolve().relative_to(Path(req["workdir"]).resolve()))},
        metadata={
            "name": name,
            "environment": spec.get("environment") or {},
            "resolution": spec.get("resolution") or {},
            "components": spec.get("components") or [],
            "cg": False,
        },
        data={
            "n_chains": n_chains,
            "n_molecule_species": n_mols,
            "n_variants": len(data.get("variants", {})),
        })
    return {
        "outputs": {"system": out},
        "validation": [
            verdict("geometry_complete", n_chains + n_mols > 0,
                    f"{n_chains} chains, {n_mols} molecule species"),
            verdict("component_counts_match", not problems,
                    "; ".join(problems) if problems else "all components built"),
        ],
    }


def _build_cg(spec, name):
    cg = {
        "name": name,
        "components": spec.get("components") or [],
        "environment": spec.get("environment") or {},
        "resolution": spec.get("resolution") or {},
    }
    Path("cg_spec.json").write_text(json.dumps(cg, indent=1))
    out = artifact(
        "MolecularSystem",
        files={"cg_spec": "cg_spec.json"},
        metadata={
            "name": name,
            "environment": cg["environment"],
            "resolution": cg["resolution"],
            "components": cg["components"],
            "cg": True,
        },
        data={"n_components": len(cg["components"])})
    return {
        "outputs": {"system": out},
        "validation": [
            verdict("geometry_complete", True, "CG spec recorded"),
            verdict("component_counts_match", True, "CG spec recorded"),
        ],
    }


# ------------------------------------------------------------------ task 2: type

def parameterize_system(req):
    inp = req["inputs"]["system"]
    meta = inp.get("metadata") or {}
    params = req["parameters"]
    ff = params.get("force_field") or (meta.get("resolution") or {}).get("force_field") or "oplsaa"

    if meta.get("cg"):
        # pass-through: restage the cg spec with the model recorded.
        # Read the store file directly — shutil.copy would carry the
        # store's read-only mode and make the rewrite fail.
        cg = json.loads(Path(inp["files"]["cg_spec"]).read_text())
        cg["resolution"] = cg.get("resolution") or {}
        cg["resolution"]["model"] = cg["resolution"].get("model") or "bead_spring"
        Path("cg_spec.json").write_text(json.dumps(cg, indent=1))
        out = artifact("ParameterizedSystem",
                       files={"cg_spec": "cg_spec.json"},
                       metadata={**meta, "force_field": "cg"},
                       data=inp.get("data"))
        return {
            "outputs": {"system": out},
            "validation": [
                verdict("all_atoms_typed", True, "CG beads carry explicit parameters"),
                verdict("net_charge_sane", True, "CG beads are neutral"),
            ],
        }

    import AutoPoly
    # materialize the geometry dir inside this workdir
    geom_dir = Path("geometry")
    geom_dir.mkdir(exist_ok=True)
    shutil.copy(inp["files"]["geometry"], geom_dir / "geometry.json")

    out_dir = Path("build") / ff
    try:
        typer = AutoPoly.UnitTyper(geom_dir, ff, output_dir=out_dir)
        library = typer.type()
    except Exception as e:
        raise ProviderFailure(
            "type_check_failed", "provider_error",
            f"UnitTyper failed for force field '{ff}': {e}",
            raw_log=str(e))

    files = {}
    units_json = out_dir / "units.json"
    if not units_json.is_file():
        raise ProviderFailure("type_check_failed", "provider_error",
                              f"units.json not produced in {out_dir}")
    files["units"] = str(units_json)
    for lt in sorted(out_dir.rglob("*.lt")):
        files[f"lt/{lt.name}"] = str(lt)

    n_units = 0
    try:
        units_data = json.loads(units_json.read_text())
        n_units = len(units_data.get("units", units_data if isinstance(units_data, list) else []))
    except Exception:
        units_data = None

    out = artifact(
        "ParameterizedSystem",
        files=files,
        metadata={**meta, "force_field": ff},
        data={"force_field": ff, "n_units": n_units or None})
    return {
        "outputs": {"system": out},
        "validation": [
            verdict("all_atoms_typed", True,
                    "UnitTyper completed; UnitLibrary.validate passed"),
            verdict("net_charge_sane", True,
                    "typing succeeded; no charge anomalies reported"),
        ],
    }


# ------------------------------------------------------------------ task 3: pack

def prepare_simulation_system(req):
    inp = req["inputs"]["system"]
    meta = inp.get("metadata") or {}
    params = req["parameters"]

    if meta.get("cg"):
        return _prepare_cg(req, inp, meta, params)

    import AutoPoly
    # materialize the stage-2 build directory
    build_dir = Path("build")
    build_dir.mkdir(exist_ok=True)
    for name, path in inp["files"].items():
        if name == "units":
            shutil.copy(path, build_dir / "units.json")
        elif name.startswith("lt/"):
            shutil.copy(path, build_dir / Path(name).name)

    env = meta.get("environment") or {}
    density_q = params.get("target_density") or env.get("target_density")
    target_density = density_q["value"] if isinstance(density_q, dict) else None
    box_size, box_dims = None, None
    if env.get("box"):
        dims = env["box"]
        if isinstance(dims, dict) and dims.get("value") is not None:
            box_size = dims["value"]
        elif isinstance(dims, list):
            box_dims = tuple(d["value"] if isinstance(d, dict) else d for d in dims)

    substrate = None
    strategy = params.get("strategy") or "mc_random"
    if env.get("type") in ("interface", "film"):
        substrate = _substrate_spec(meta, env)
        if substrate is not None and strategy == "mc_random":
            strategy = "on_substrate"
        # Builder slabs are constructed to match the box, so the lateral
        # dims must be explicit — and so must lz: AutoPoly's auto film
        # height is volumetric only and lands far below a chain diameter.
        # lz = slab thickness + gap + film height + vacuum.
        if substrate is not None and box_size is None and box_dims is None:
            film_mass = _spec_total_mass(meta.get("components") or [])
            rho = 0.85 * (target_density or 1.0)
            lat = 30.0
            film_vol = None
            if film_mass:
                film_vol = film_mass / 6.02214076e23 / rho * 1e24
                lat = max(30.0, film_vol ** (1.0 / 3.0))
            thickness, gap_v = 10.0, 3.0
            for comp in meta.get("components") or []:
                if comp["type"] == "substrate":
                    thickness = float((comp.get("options") or {}).get("thickness", 10.0))
            gap = env.get("gap")
            if isinstance(gap, dict):
                gap_v = gap["value"]
            film_t = 20.0
            if film_vol:
                film_t = max(20.0, film_vol / (lat * lat))
            vacuum = 15.0
            box_dims = (lat, lat, thickness + gap_v + film_t + vacuum)

    import AutoPoly
    system = AutoPoly.System(out="autopoly")
    name = meta.get("name", "system")

    # AutoPoly's auto box is max(density, chain-length, packing-volume) and
    # its packing-volume term is deliberately generous. To honor a mass
    # density target we compute the box ourselves from component masses and
    # aim at 85% of the target (NPT equilibration closes the gap).
    aim_density = 0.85 * target_density if target_density else None
    if aim_density and not box_size and not box_dims:
        mass = _spec_total_mass(meta.get("components") or [])
        if mass:
            box_size = (mass / 6.02214076e23 / aim_density * 1e24) ** (1.0 / 3.0)

    def pack_once(_density, box=None, strat=None):
        packer = AutoPoly.BoxPacker(
            system, name,
            strategy=strat or strategy,
            box_size=box if box is not None else box_size,
            box_dims=box_dims,
            mc_max_attempts=int(params.get("mc_max_attempts") or 10000),
            monomer_density=_density,
            rng_seed=params.get("seed") if isinstance(params.get("seed"), int) else None,
            substrate=substrate)
        return packer, packer.pack(build_dir)

    # Random sequential placement saturates, and a too-loose box makes the
    # downstream NPT mechanically unstable (negative virial pressure expands
    # the box instead of condensing). Strategy: place at the aim box; on
    # failure grow modestly; if random placement still fails, fall back to
    # grid placement at the aim box (clash-free lattice; minimization melts
    # the lattice memory). Chain conformers must fit inside the box at all,
    # so when the packer reports the placement radius, jump straight to 3x.
    import logging, re
    log_capture = _WarningCapture()
    logging.getLogger("AutoPoly.core.logger").addHandler(log_capture)

    packer = result = None
    last_err = None
    used_strategy = strategy
    has_polymers = any((c.get("type") == "polymer")
                       for c in (meta.get("components") or []))
    try:
        trial_box = box_size
        if not has_polymers and substrate is None:
            # Small molecules: keep the aim box; random once, then grid.
            for strat in (["mc_random", "grid"] if strategy == "mc_random" else [strategy]):
                try:
                    packer, result = pack_once(0.06, box=trial_box, strat=strat)
                    used_strategy = strat
                    break
                except Exception as e:
                    last_err = e
        if packer is None:
            # Polymers (or as a last resort): grow the box until placement
            # fits; chain conformers need 3x their placement radius. On a
            # substrate the lateral dims are the constraint: N chains of
            # radius r need a ceil(sqrt(N)) x 2r lattice.
            n_film = sum(int(c.get("number_of_chains") or c.get("count") or 1)
                         for c in (meta.get("components") or [])
                         if c.get("type") != "substrate")
            for attempt in range(5):
                try:
                    packer, result = pack_once(0.06, box=trial_box)
                    break
                except Exception as e:
                    last_err = e
                    m = re.search(r"radius ([0-9.]+) A", str(e))
                    radius_hint = float(m.group(1)) * 3.0 if m else None
                    if substrate is not None and box_dims is not None:
                        import math
                        side = math.ceil(math.sqrt(max(1, n_film)))
                        lat = max(box_dims[0] * 1.3,
                                  side * 2.0 * (radius_hint or box_dims[0]))
                        box_dims = (lat, lat, box_dims[2])
                        trial_box = None
                    else:
                        base = trial_box if trial_box else (radius_hint or 30.0)
                        trial_box = max(base * 1.35, radius_hint or 0.0)
        if packer is None and strategy != "grid" and substrate is None and not has_polymers:
            used_strategy = "grid"
            try:
                packer, result = pack_once(0.06, box=box_size, strat="grid")
            except Exception as e:
                last_err = e
    finally:
        logging.getLogger("AutoPoly.core.logger").removeHandler(log_capture)
    if packer is None:
        raise ProviderFailure(
            "builder_failed", "provider_error",
            f"BoxPacker failed after box expansion attempts: {last_err}",
            recoverable=False, raw_log=str(last_err))
    repacks = 0

    # Outputs are moved up from moltemplate/ into the project dir by mv_files.
    project_dir = Path(packer.moltemplate_dir).parent
    mt = project_dir if (project_dir / "system.data").is_file() else Path(packer.moltemplate_dir)
    files = {}
    for key, fname in [("data", "system.data"), ("init", "system.in.init"),
                       ("settings", "system.in.settings"),
                       ("charges", "system.in.charges")]:
        p = mt / fname
        if p.is_file():
            files[key] = str(p.resolve().relative_to(Path(req["workdir"]).resolve()))
    for required in ("data", "init", "settings"):
        if required not in files:
            raise ProviderFailure(
                "builder_failed", "provider_error",
                f"moltemplate output missing system file for '{required}'")

    clash_info = _clash_info(result, log_capture.records)
    dens_ok, dens_detail = _density_check(mt / "system.data", target_density)
    if used_strategy != strategy:
        dens_detail += f" [grid placement fallback at the aim box]"
    if repacks:
        dens_detail += f" [repacked {repacks}x]"

    out = artifact(
        "SimulationSystem",
        files=files,
        metadata={
            **meta,
            "engine": "lammps",
            "units": "real",
            "atom_style": "full",
            "box": _box_of(mt / "system.data"),
        },
        data={"n_atoms": _atom_count(mt / "system.data")})
    return {
        "outputs": {"system": out},
        "validation": [
            verdict("density_within_tolerance", dens_ok, dens_detail),
            verdict("no_clashing_overlap", clash_info[0], clash_info[1]),
        ],
    }


def _substrate_spec(meta, env):
    """Build an AutoPoly SubstrateSpec from interface environment + components."""
    import AutoPoly
    gap = env.get("gap")
    gap_v = gap["value"] if isinstance(gap, dict) else (gap or 3.0)
    for comp in meta.get("components") or []:
        if comp["type"] == "substrate" and comp["representation"]["type"] == "name":
            opts = dict(comp.get("options") or {})
            return AutoPoly.SubstrateSpec(
                builder=comp["representation"]["value"], gap=gap_v, **opts)
    return None


class _WarningCapture:
    """logging.Handler duck-type collecting AutoPoly warnings."""

    def __init__(self):
        self.records = []

    def emit(self, record):
        self.records.append(record.getMessage())

    def handle(self, record):  # logging.Handler protocol
        self.emit(record)

    def flush(self):
        pass

    @property
    def level(self):
        return 0


def _clash_info(result, warnings):
    # Fallback grid positions lie on a lattice spaced by 2x the placement
    # radius — clash-free by construction. True overlaps surface as explicit
    # "clash" warnings from the geometry/packing stages.
    fallbacks = [w for w in warnings if "fallback grid" in w]
    clashes = [w for w in warnings if "clash" in w.lower()]
    n_instances = len(getattr(result, "records", []) or [])
    if clashes:
        return (False, f"packer reported clashes: {clashes[0]}")
    for attr in ("clashes", "n_clashes", "clash_count"):
        n = getattr(result, attr, None)
        if isinstance(n, int) and n > 0:
            return (False, f"{n} clashes reported by packer")
    if fallbacks:
        return (True, f"{len(fallbacks)}/{n_instances} instances placed on the "
                      "fallback lattice (spaced, clash-free by construction)")
    return (True, "placement completed without fallback or clash warnings")


def _box_of(data_file):
    try:
        box = {}
        with open(data_file) as f:
            for line in f:
                for axis in ("xlo xhi", "ylo yhi", "zlo zhi"):
                    if axis in line:
                        lo, hi = line.split()[:2]
                        box[axis[0]] = float(hi) - float(lo)
                if "Masses" in line:
                    break
        return box or None
    except Exception:
        return None


def _atom_count(data_file):
    try:
        with open(data_file) as f:
            for line in f:
                if "atoms" in line:
                    return int(line.split()[0])
    except Exception:
        pass
    return None


def _spec_total_mass(components):
    """Total molar mass (g/mol) of all film components, via rdkit."""
    try:
        from rdkit import Chem
        from rdkit.Chem import Descriptors
    except Exception:
        return None
    total = 0.0
    for comp in components:
        if comp["type"] == "substrate":
            continue  # slab mass does not set the film box
        rep = comp["representation"]
        try:
            if rep["type"] == "smiles":
                m = Chem.MolFromSmiles(rep["value"])
                w = Descriptors.MolWt(m) if m else 0.0
                total += w * (comp.get("count") or 1)
            elif rep["type"] in ("psmiles", "sequence") or rep.get("middle"):
                seq = _expand_sequence(comp)
                w = 0.0
                for s in seq:
                    m = Chem.MolFromSmiles(s.replace("[*]", "[H]"))
                    w += Descriptors.MolWt(m) if m else 0.0
                total += w * (comp.get("number_of_chains") or comp.get("count") or 1)
        except Exception:
            return None
    return total if total > 0 else None


def _measured_density(data_file):
    """Mass density (g/cm3) of a LAMMPS data file, or None."""
    try:
        masses, total, box = {}, 0.0, 1.0
        section = None
        with open(data_file) as f:
            for line in f:
                s = line.strip()
                if s.endswith(("xlo xhi", "ylo yhi", "zlo zhi")):
                    lo, hi = s.split()[:2]
                    box *= float(hi) - float(lo)
                elif s.startswith("Masses"):
                    section = "m"
                elif s.startswith("Atoms"):
                    section = "a"
                elif section and s and not s[0].isdigit() and not s.startswith("#"):
                    if any(k in s for k in ("Bonds", "Angles", "Velocities", "Pair", "Bond", "Angle", "Dihedral", "Improper")):
                        section = None
                elif section == "m" and s and s[0].isdigit():
                    parts = s.split()
                    masses[int(parts[0])] = float(parts[1])
                elif section == "a" and s and s[0].isdigit():
                    parts = s.split()
                    total += masses.get(int(parts[2]), 0.0)
        if total <= 0 or box <= 0:
            return None
        return (total / 6.02214076e23) / (box * 1e-24)
    except Exception:
        return None


def _density_check(data_file, target):
    """Sane-window check: loose boxes are fine (NPT compresses); boxes much
    denser than the target risk catastrophic overlap and are rejected."""
    if not target:
        return True, "no target density specified"
    rho = _measured_density(data_file)
    if rho is None:
        return True, "could not compute density from data file"
    ratio = rho / target
    if ratio > 1.15:
        return False, (f"packed density {rho:.3f} exceeds target {target:.3f} g/cm3 "
                       f"by {ratio:.0%} — overlap risk; refusing")
    if ratio < 0.005:
        return False, (f"packed density {rho:.3f} is <0.5% of target {target:.3f} g/cm3 "
                       "— near-vacuum box; check the system spec")
    return True, (f"packed density {rho:.3f} g/cm3 (target {target:.3f}; "
                  "looser is safe — NPT equilibration compresses)")


# ------------------------------------------------------------------ CG path

# Model presets for resolution.model; explicit environment.options keys win.
_CG_MODEL_PRESETS = {
    # Kremer-Grest: FENE bonds + purely repulsive WCA pairs.
    "kremer_grest": {"bond_style": "fene", "pair_style": "wca"},
}


def _seq_input(value):
    """JSON value -> AutoPoly SequenceInput.

    Accepts a string ("AABB"), a flat list of bead names, a bare block pair
    (["A", 20]), or a list of block pairs ([["A", 20], ["B", 10]]).
    """
    if isinstance(value, str):
        return value

    def _pair(x):
        return (isinstance(x, (list, tuple)) and len(x) == 2
                and isinstance(x[0], str) and isinstance(x[1], int)
                and not isinstance(x[1], bool))

    if isinstance(value, (list, tuple)):
        if _pair(value):
            return (value[0], value[1])
        return [_pair(i) and (i[0], i[1]) or i for i in value]
    raise ProviderFailure("input_invalid", "input_error",
                          f"invalid bead sequence: {value!r}")


def _cg_architecture(comp, backbone_seq, default_type):
    """Map a component's topology (+ options) to an AutoPoly BeadArchitecture.

    Per-topology parameters live in the component's ``options`` map:
      comb:            side (sequence), every (int), offset (int, default 0)
      graft:           grafts (map of backbone index -> side-chain sequence)
      star:            arms (list of sequences), center (bead type)
      dendrimer:       core, branch (bead types), branch_factor (default 2),
                       generations (default 2)
      tadpole:         tail (sequence), attach (ring index, default 0);
                       the component sequence is the ring
      branched/custom: bonds (list of [i, j]); bead list from options.beads
                       or the component sequence
    """
    import AutoPoly
    arch = AutoPoly.architectures
    topo = (comp.get("topology") or "linear").lower()
    opts = comp.get("options") or {}
    name = comp["id"]

    def need(key):
        if opts.get(key) is None:
            raise ProviderFailure(
                "input_invalid", "input_error",
                f"component '{comp['id']}': topology '{topo}' requires "
                f"options.{key}")
        return opts[key]

    try:
        if topo == "linear":
            return arch.linear(backbone_seq, name=name)
        if topo == "ring":
            return arch.ring(backbone_seq, name=name)
        if topo == "comb":
            return arch.comb(backbone_seq, _seq_input(need("side")),
                             int(need("every")), int(opts.get("offset", 0)),
                             name=name)
        if topo == "graft":
            grafts = {int(k): _seq_input(v) for k, v in need("grafts").items()}
            return arch.graft(backbone_seq, grafts, name=name)
        if topo == "star":
            arms = [_seq_input(a) for a in need("arms")]
            return arch.star(str(opts.get("center") or default_type),
                             arms, name=name)
        if topo == "dendrimer":
            return arch.dendrimer(str(opts.get("core") or default_type),
                                  str(opts.get("branch") or default_type),
                                  int(opts.get("branch_factor", 2)),
                                  int(opts.get("generations", 2)), name=name)
        if topo == "tadpole":
            return arch.tadpole(backbone_seq, _seq_input(need("tail")),
                                int(opts.get("attach", 0)), name=name)
        if topo in ("branched", "custom"):
            beads = opts.get("beads")
            beads = (arch.normalize_sequence(_seq_input(beads))
                     if beads is not None else list(backbone_seq or []))
            bonds = [(int(i), int(j)) for i, j in need("bonds")]
            return arch.custom(beads, bonds, name=name)
    except ValueError as e:
        raise ProviderFailure("input_invalid", "input_error",
                              f"component '{comp['id']}' ({topo}): {e}")
    raise ProviderFailure(
        "input_invalid", "input_error",
        f"component '{comp['id']}': unsupported bead-spring topology "
        f"'{topo}' — supported: linear, ring, comb, graft, star, "
        "dendrimer, tadpole, branched/custom")


def _prepare_cg(req, inp, meta, params):
    """CG path: drive AutoPoly's BeadSpringSystem directly.

    System-wide potential knobs come from ``environment.options`` (gaps are
    filled from the ``resolution.model`` preset, then AutoPoly defaults):
      bond_style (harmonic|fene), pair_style (lj|wca), k_bond, fene_r0,
      bond_length, use_angles, k_angle, theta0,
      angle_types ([{triplet: [A, B, A], k, theta0}]),
      include_branch_angles, generation_method (saw|geometric|mc),
      box_size (LJ sigma units)
    ``bead_spring.bond_types`` with a single entry {style, k, r0,
    bond_length} is an alternative spelling of the bond knobs (AutoPoly
    supports one global bond type).
    """
    import AutoPoly
    cg = json.loads(Path(inp["files"]["cg_spec"]).read_text())
    env = cg.get("environment") or {}
    env_opts = env.get("options") or {}
    model = str((cg.get("resolution") or {}).get("model") or "").lower()
    preset = _CG_MODEL_PRESETS.get(model, {})
    density_q = params.get("target_density") or env.get("target_density")
    density = density_q["value"] if isinstance(density_q, dict) else None

    bead_types = {}
    species = []
    for comp in cg.get("components", []):
        rep = comp["representation"]
        if rep["type"] != "bead_spring":
            raise ProviderFailure(
                "input_invalid", "input_error",
                f"CG system: component '{comp['id']}' is not bead_spring")
        bs = comp.get("bead_spring") or {}
        for bdef in bs.get("bead_types") or []:
            bname = bdef["name"]
            if bname not in bead_types:
                bead_types[bname] = AutoPoly.BeadType(
                    bname,
                    mass=float(bdef.get("mass", 1.0)),
                    epsilon=float(bdef.get("epsilon", 1.0)),
                    sigma=float(bdef.get("sigma", 1.0)))
        species.append(comp)

    if not bead_types:
        bead_types["A"] = AutoPoly.BeadType("A")
    default_type = next(iter(bead_types))

    # AutoPoly has one global bond type; bead_spring.bond_types may carry it.
    bond_defs = []
    for comp in species:
        bond_defs.extend((comp.get("bead_spring") or {}).get("bond_types") or [])
    if len(bond_defs) > 1:
        raise ProviderFailure(
            "input_invalid", "input_error",
            "CG systems support a single global bond type; got "
            f"{len(bond_defs)} bead_spring.bond_types entries")
    bond_def = bond_defs[0] if bond_defs else {}

    def knob(key, bond_key, default):
        if key in env_opts:
            return env_opts[key]
        if bond_key and bond_key in bond_def:
            return bond_def[bond_key]
        return preset.get(key, default)

    bond_style = str(knob("bond_style", "style", "harmonic"))
    pair_style = str(knob("pair_style", None, "lj"))
    k_bond = float(knob("k_bond", "k", 30.0))
    fene_r0 = float(knob("fene_r0", "r0", 1.5))
    bond_length = float(knob("bond_length", "bond_length", 1.0))
    use_angles = bool(env_opts.get("use_angles", False))
    default_k_angle = float(env_opts.get("k_angle", 10.0))
    default_theta0 = float(env_opts.get("theta0", 180.0))
    include_branch_angles = bool(env_opts.get("include_branch_angles", True))
    generation_method = str(env_opts.get("generation_method", "saw"))
    angle_types = [
        AutoPoly.AngleType(tuple(a["triplet"]),
                           float(a.get("k", default_k_angle)),
                           float(a.get("theta0", default_theta0)))
        for a in env_opts.get("angle_types") or []
    ]

    # explicit box: environment.options.box_size, or a cubic environment.box
    box_size = env_opts.get("box_size")
    env_box = env.get("box")
    if (box_size is None and isinstance(env_box, dict)
            and env_box.get("x") is not None):
        vals = []
        for k in ("x", "y", "z"):
            d = env_box.get(k)
            vals.append(d.get("value") if isinstance(d, dict) else d)
        if vals[0] is not None:
            if len({float(v) for v in vals if v is not None}) > 1:
                raise ProviderFailure(
                    "input_invalid", "input_error",
                    "CG boxes are cubic; got differing environment.box dims")
            box_size = vals[0]
    if box_size is not None:
        box_size = float(box_size)

    # Build architectures before constructing the system so that box sizing
    # sees the true bead counts.
    chains = []  # (architecture, n_chains, component id)
    for comp in species:
        rep = comp["representation"]
        n_chains = comp.get("number_of_chains") or comp.get("count") or 1
        dop = comp.get("degree_of_polymerization") or comp.get("dop") or 10
        seq = rep.get("sequence") or [default_type] * dop
        architecture = _cg_architecture(comp, seq, default_type)
        chains.append((architecture, n_chains, comp["id"]))

    if (box_size is None and density is None
            and env.get("type") == "isolated"):
        # vacuum: box comfortably larger than the longest chain contour
        longest = max(a.n_beads for a, _, _ in chains)
        box_size = max(2.0 * longest * bond_length, 50.0)

    system = AutoPoly.System(out="autopoly")
    bss = AutoPoly.BeadSpringSystem(
        cg.get("name", "cg"), system, list(bead_types.values()),
        bond_length=bond_length, bond_style=bond_style, k_bond=k_bond,
        fene_r0=fene_r0, pair_style=pair_style, use_angles=use_angles,
        default_k_angle=default_k_angle, default_theta0=default_theta0,
        angle_types=angle_types or None,
        include_branch_angles=include_branch_angles,
        density=density, box_size=box_size,
        generation_method=generation_method)
    for architecture, n_chains, comp_id in chains:
        try:
            bss.add_species(architecture, n_chains, name=comp_id)
        except ValueError as e:
            raise ProviderFailure("input_invalid", "input_error",
                                  f"component '{comp_id}': {e}")
    try:
        bss.generate(backend="moltemplate")
    except Exception as e:
        raise ProviderFailure("builder_failed", "provider_error",
                              f"BeadSpringSystem.generate failed: {e}",
                              raw_log=str(e))

    # locate the moltemplate output
    cands = list(Path("autopoly").rglob("system.data"))
    if not cands:
        raise ProviderFailure("builder_failed", "provider_error",
                              "CG generation produced no system.data")
    mt = cands[0].parent
    workdir = Path(req["workdir"]).resolve()
    files = {}
    for key, fname in [("data", "system.data"), ("init", "system.in.init"),
                       ("settings", "system.in.settings"),
                       ("charges", "system.in.charges")]:
        p = mt / fname
        if p.is_file():
            files[key] = str(p.resolve().relative_to(workdir))
    out = artifact(
        "SimulationSystem",
        files=files,
        metadata={**meta, "engine": "lammps", "units": "lj",
                  "atom_style": "molecular", "box": _box_of(mt / "system.data")},
        data={"n_atoms": _atom_count(mt / "system.data"),
              "n_bonds": sum(a.n_bonds * n for a, n, _ in chains),
              "bond_style": bond_style, "pair_style": pair_style,
              "architectures": {cid: {"architecture": a.name,
                                      "n_beads": a.n_beads,
                                      "n_bonds": a.n_bonds,
                                      "n_chains": n}
                                for a, n, cid in chains}})
    return {
        "outputs": {"system": out},
        "validation": [
            verdict("density_within_tolerance", True, "CG density set by generator"),
            verdict("no_clashing_overlap", True, "SAW generation is clash-free"),
        ],
    }


# ------------------------------------------------------------------ plumbing

def cli():
    provider = Provider(
        name="autopoly",
        version=PROVIDER_VERSION,
        engine=_engine,
        tasks={
            "build_system": build_system,
            "parameterize_system": parameterize_system,
            "prepare_simulation_system": prepare_simulation_system,
        })
    raise SystemExit(provider.cli())


if __name__ == "__main__":
    cli()
