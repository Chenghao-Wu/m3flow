//! Artifact type hierarchy (plan §8).
//!
//! Four scientific families — System, State, Dataset, Result — plus Spec for
//! user-authored inputs (so provenance chains can start at a SystemSpec) and
//! the open root `Artifact`. Compatibility is nominal subtyping: a value of a
//! subtype is accepted wherever a supertype is required.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Spec,
    System,
    State,
    Dataset,
    Result,
    Root,
}

/// parent-of table; every type has exactly one parent (tree).
const HIERARCHY: &[(&str, &str)] = &[
    ("Spec", "Artifact"),
    ("SystemSpec", "Spec"),
    ("System", "Artifact"),
    ("MolecularSystem", "System"),
    ("ParameterizedSystem", "System"),
    ("SimulationSystem", "System"),
    ("State", "Artifact"),
    ("SimulationState", "State"),
    ("EquilibratedState", "SimulationState"),
    ("Dataset", "Artifact"),
    ("Trajectory", "Dataset"),
    ("ProductionTrajectory", "Trajectory"),
    ("SimulationLog", "Dataset"),
    ("ThermodynamicSeries", "Dataset"),
    ("TemperatureSeries", "ThermodynamicSeries"),
    ("StressStrainSeries", "Dataset"),
    ("Result", "Artifact"),
    ("DensityResult", "Result"),
    ("RDFResult", "Result"),
    ("RgResult", "Result"),
    ("ReeResult", "Result"),
    ("MSDResult", "Result"),
    ("DiffusionResult", "Result"),
    ("CTEResult", "Result"),
    ("TgResult", "Result"),
    ("AdhesionResult", "Result"),
    ("ModulusResult", "Result"),
    ("EquilibrationReport", "Result"),
];

fn parent_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| HIERARCHY.iter().copied().collect())
}

pub fn is_known_type(t: &str) -> bool {
    t == "Artifact" || parent_map().contains_key(t)
}

/// Immediate parent in the type hierarchy (None for the root).
pub fn parent_of(t: &str) -> Option<&'static str> {
    parent_map().get(t).copied()
}

/// True if `have` can be used where `want` is required (have ≤ want).
pub fn is_subtype(have: &str, want: &str) -> bool {
    let mut cur = have;
    loop {
        if cur == want {
            return true;
        }
        match parent_map().get(cur) {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

pub fn family_of(t: &str) -> Family {
    for (fam, f) in [
        (Family::Spec, "Spec"),
        (Family::System, "System"),
        (Family::State, "State"),
        (Family::Dataset, "Dataset"),
        (Family::Result, "Result"),
    ] {
        if is_subtype(t, f) {
            return fam;
        }
    }
    Family::Root
}

/// All registered type names, root first (for `schema list`/docs).
pub fn all_types() -> Vec<&'static str> {
    let mut v = vec!["Artifact"];
    v.extend(parent_map().keys().copied());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtyping() {
        assert!(is_subtype("EquilibratedState", "SimulationState"));
        assert!(is_subtype("EquilibratedState", "State"));
        assert!(is_subtype("EquilibratedState", "Artifact"));
        assert!(is_subtype("ProductionTrajectory", "Trajectory"));
        assert!(is_subtype("TemperatureSeries", "ThermodynamicSeries"));
        assert!(!is_subtype("SimulationState", "EquilibratedState"));
        assert!(!is_subtype("Trajectory", "Result"));
        assert!(!is_subtype("Bogus", "Artifact"));
    }

    #[test]
    fn families() {
        assert_eq!(family_of("EquilibratedState"), Family::State);
        assert_eq!(family_of("DensityResult"), Family::Result);
        assert_eq!(family_of("SystemSpec"), Family::Spec);
        assert_eq!(family_of("SimulationSystem"), Family::System);
    }
}
