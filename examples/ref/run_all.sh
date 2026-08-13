#!/usr/bin/env bash
# Reproduce all five reduced-scale reference runs (plan §66/67).
# Usage: cd examples/ref && ./run_all.sh [path-to-m3flow]
set -euo pipefail
M3FLOW="${1:-$(pwd)/../../target/release/m3flow}"

echo "== 1/5 ethanol_diffusion"
$M3FLOW workflow run ethanol_diffusion --input specification=@systems/ethanol.yaml

echo "== 2/5 peo_density"
$M3FLOW workflow run peo_density --input specification=@systems/peo_small.yaml

echo "== 3/5 polymer_multi (run twice; second run proves the cache)"
$M3FLOW workflow run polymer_multi --input specification=@systems/peo_small.yaml
$M3FLOW workflow run polymer_multi --input specification=@systems/peo_small.yaml

echo "== 4/5 peo_silica_adhesion (group selectors depend on typing; see systems/peo_on_silica.yaml comments)"
$M3FLOW workflow run peo_silica_adhesion --input specification=@systems/peo_on_silica.yaml \
  --param 'interface_area={value: 1494, unit: angstrom2}' \
  --param 'group_a="type 1 2 3 4 5 6 7 8"' --param 'group_b="type 9 10 11 12"'

echo "== 5/5 cg_melt"
$M3FLOW workflow run cg_melt --input specification=@systems/cg_melt.yaml

echo "all reference runs COMPLETED"
