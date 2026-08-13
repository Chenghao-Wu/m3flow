"""placeholder — replaced by the real analysis provider (Phase 6)."""
from m3flow_provider import Provider

def cli():
    raise SystemExit(Provider("analysis", "0.0.0",
                              lambda: {"name": "mdanalysis", "version": "unknown"},
                              {}).cli())
