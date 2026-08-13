"""placeholder — replaced by the real LAMMPS provider (Phase 3)."""
from m3flow_provider import Provider

def cli():
    raise SystemExit(Provider("lammps", "0.0.0",
                              lambda: {"name": "lammps", "version": "unknown"},
                              {}).cli())
