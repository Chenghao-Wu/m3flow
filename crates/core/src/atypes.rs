//! Artifact type hierarchy (plan §8).
//!
//! Four scientific families — System, State, Dataset, Result — plus Spec for
//! user-authored inputs (so provenance chains can start at a SystemSpec) and
//! the open root `Artifact`. Compatibility is nominal subtyping: a value of a
//! subtype is accepted wherever a supertype is required.
//!
//! The compiled-in table below is the standard library; it is frozen per
//! release (a type that ships builtin stays builtin — moving one to a pack
//! would break existing projects). Projects extend the hierarchy
//! declaratively with `types/v1` pack documents (see the registry crate);
//! extension entries may only attach under an already-known parent, so the
//! tree grows downward and cycles are impossible by construction.

use crate::error::{M3FlowError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// builtin parent-of table; every type has exactly one parent (tree).
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

/// Certified types and the only task allowed to create them. A certified
/// artifact (or one of a subtype) may not be registered by hand or emitted
/// by any other task; the runtime additionally verifies the evidence
/// binding of each creation.
pub const PROTECTED_TYPES: &[(&str, &str)] = &[("EquilibratedState", "promote_equilibrated_state")];

#[derive(Debug, Clone)]
struct TypeInfo {
    parent: String,
    /// where the type was defined ("<builtin>" or a pack origin), for diagnostics
    origin: String,
}

/// A merged view of the builtin hierarchy plus any extension packs.
///
/// Queries are pure tree walks; adding extension nodes never changes the
/// answer for a pair of builtin types (their ancestry passes only through
/// builtin parents).
#[derive(Debug, Clone)]
pub struct TypeSet {
    /// name -> info; `Artifact` is the implicit root (not stored)
    types: HashMap<String, TypeInfo>,
    /// registration order, for deterministic `schema list` output
    order: Vec<String>,
}

impl Default for TypeSet {
    fn default() -> Self {
        Self::builtins()
    }
}

impl TypeSet {
    /// The compiled-in standard library, no extensions.
    pub fn builtins() -> Self {
        let mut ts = Self {
            types: HashMap::new(),
            order: Vec::new(),
        };
        for (name, parent) in HIERARCHY {
            ts.types.insert(
                name.to_string(),
                TypeInfo {
                    parent: parent.to_string(),
                    origin: "<builtin>".to_string(),
                },
            );
            ts.order.push(name.to_string());
        }
        ts
    }

    pub fn is_known_type(&self, t: &str) -> bool {
        t == "Artifact" || self.types.contains_key(t)
    }

    /// Immediate parent in the type hierarchy (None for the root/unknown).
    pub fn parent_of(&self, t: &str) -> Option<&str> {
        self.types.get(t).map(|i| i.parent.as_str())
    }

    /// Where a type was defined (`<builtin>` or a pack origin).
    pub fn origin_of(&self, t: &str) -> Option<&str> {
        self.types.get(t).map(|i| i.origin.as_str())
    }

    /// True if `have` can be used where `want` is required (have ≤ want).
    pub fn is_subtype(&self, have: &str, want: &str) -> bool {
        let mut cur = have;
        loop {
            if cur == want {
                return true;
            }
            match self.types.get(cur) {
                Some(i) => cur = i.parent.as_str(),
                None => return false,
            }
        }
    }

    /// The protected type `t` falls under (itself or an ancestor) and the
    /// task that alone may create it.
    pub fn protected(&self, t: &str) -> Option<(&'static str, &'static str)> {
        PROTECTED_TYPES
            .iter()
            .find(|(p, _)| self.is_subtype(t, p))
            .copied()
    }

    pub fn family_of(&self, t: &str) -> Family {
        for (fam, f) in [
            (Family::Spec, "Spec"),
            (Family::System, "System"),
            (Family::State, "State"),
            (Family::Dataset, "Dataset"),
            (Family::Result, "Result"),
        ] {
            if self.is_subtype(t, f) {
                return fam;
            }
        }
        Family::Root
    }

    /// All registered type names, root first, in registration order
    /// (for `schema list`/docs).
    pub fn all_types(&self) -> Vec<&str> {
        let mut v = vec!["Artifact"];
        v.extend(self.order.iter().map(|s| s.as_str()));
        v
    }

    /// Register one extension type. The parent must already be known — the
    /// tree only grows downward, so cycles cannot form. Type names are global
    /// identifiers inside artifact records and provenance chains, so
    /// redefinition is a hard error, never an override.
    pub fn add(&mut self, name: &str, parent: &str, origin: &str) -> Result<()> {
        if !valid_type_name(name) {
            return Err(M3FlowError::schema(format!(
                "{origin}: invalid type name '{name}' (want [A-Z][A-Za-z0-9]*)"
            )));
        }
        if self.is_known_type(name) {
            let existing = self.origin_of(name).unwrap_or("<root>");
            return Err(M3FlowError::schema(format!(
                "{origin}: type '{name}' is already defined by {existing} (types cannot be redefined)"
            )));
        }
        if !self.is_known_type(parent) {
            return Err(M3FlowError::schema(format!(
                "{origin}: parent '{parent}' of '{name}' is not a known type"
            )));
        }
        self.types.insert(
            name.to_string(),
            TypeInfo {
                parent: parent.to_string(),
                origin: origin.to_string(),
            },
        );
        self.order.push(name.to_string());
        Ok(())
    }

    /// Register a pack's entries regardless of declaration order: intra-pack
    /// parent chains (e.g. `LabeledStructureSet <: StructureSet` appearing
    /// alphabetically before its parent) resolve by fixpoint. An entry whose
    /// parent no entry provides and no prior type defines is an error.
    pub fn add_pack(&mut self, entries: &[(String, String)], origin: &str) -> Result<()> {
        let mut pending: Vec<&(String, String)> = entries.iter().collect();
        loop {
            let mut progress = false;
            let mut i = 0;
            while i < pending.len() {
                let (name, parent) = &pending[i];
                if self.is_known_type(parent) {
                    self.add(name, parent, origin)?;
                    pending.remove(i);
                    progress = true;
                } else {
                    i += 1;
                }
            }
            if pending.is_empty() {
                return Ok(());
            }
            if !progress {
                let stuck: Vec<String> = pending
                    .iter()
                    .map(|(n, p)| format!("{n} (parent {p})"))
                    .collect();
                return Err(M3FlowError::schema(format!(
                    "{origin}: unresolvable parent types: {}",
                    stuck.join(", ")
                )));
            }
        }
    }
}

/// Type names are flat CamelCase identifiers: `[A-Z][A-Za-z0-9]*`.
fn valid_type_name(n: &str) -> bool {
    let mut chars = n.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtyping() {
        let ts = TypeSet::builtins();
        assert!(ts.is_subtype("EquilibratedState", "SimulationState"));
        assert!(ts.is_subtype("EquilibratedState", "State"));
        assert!(ts.is_subtype("EquilibratedState", "Artifact"));
        assert!(ts.is_subtype("ProductionTrajectory", "Trajectory"));
        assert!(ts.is_subtype("TemperatureSeries", "ThermodynamicSeries"));
        assert!(!ts.is_subtype("SimulationState", "EquilibratedState"));
        assert!(!ts.is_subtype("Trajectory", "Result"));
        assert!(!ts.is_subtype("Bogus", "Artifact"));
    }

    #[test]
    fn certified_types_are_protected_including_subtypes() {
        let mut ts = TypeSet::builtins();
        assert_eq!(
            ts.protected("EquilibratedState").map(|(_, task)| task),
            Some("promote_equilibrated_state")
        );
        assert!(ts.protected("SimulationState").is_none());
        ts.add("AnnealedState", "EquilibratedState", "pack")
            .unwrap();
        assert!(ts.protected("AnnealedState").is_some());
    }

    #[test]
    fn families() {
        let ts = TypeSet::builtins();
        assert_eq!(ts.family_of("EquilibratedState"), Family::State);
        assert_eq!(ts.family_of("DensityResult"), Family::Result);
        assert_eq!(ts.family_of("SystemSpec"), Family::Spec);
        assert_eq!(ts.family_of("SimulationSystem"), Family::System);
    }

    fn atomicsim_entries() -> Vec<(String, String)> {
        // deliberately declaration-ordered child-before-parent in one spot
        vec![
            ("AtomicStructure".into(), "System".into()),
            ("LabeledStructureSet".into(), "StructureSet".into()),
            ("StructureSet".into(), "Dataset".into()),
            ("RelaxedStructure".into(), "AtomicStructure".into()),
            ("RelaxationResult".into(), "Result".into()),
        ]
    }

    #[test]
    fn pack_entries_resolve_out_of_order() {
        let mut ts = TypeSet::builtins();
        ts.add_pack(&atomicsim_entries(), "test-pack").unwrap();
        assert!(ts.is_subtype("RelaxedStructure", "AtomicStructure"));
        assert!(ts.is_subtype("RelaxedStructure", "System"));
        assert!(ts.is_subtype("RelaxedStructure", "Artifact"));
        assert!(ts.is_subtype("LabeledStructureSet", "Dataset"));
        assert_eq!(ts.family_of("RelaxedStructure"), Family::System);
        assert_eq!(ts.family_of("RelaxationResult"), Family::Result);
        assert_eq!(ts.parent_of("StructureSet"), Some("Dataset"));
        assert_eq!(ts.origin_of("StructureSet"), Some("test-pack"));
    }

    #[test]
    fn extension_never_changes_builtin_answers() {
        let base = TypeSet::builtins();
        let mut ext = base.clone();
        ext.add_pack(&atomicsim_entries(), "test-pack").unwrap();
        for have in base.all_types() {
            for want in base.all_types() {
                assert_eq!(
                    base.is_subtype(have, want),
                    ext.is_subtype(have, want),
                    "{have} <: {want} changed after pack load"
                );
            }
        }
    }

    #[test]
    fn redefinition_is_an_error() {
        let mut ts = TypeSet::builtins();
        let e = ts.add("DensityResult", "Result", "evil-pack").unwrap_err();
        assert!(e.to_string().contains("already defined by <builtin>"));
        ts.add_pack(&atomicsim_entries(), "test-pack").unwrap();
        let e = ts.add("StructureSet", "Dataset", "other-pack").unwrap_err();
        assert!(e.to_string().contains("already defined by test-pack"));
    }

    #[test]
    fn unknown_parent_is_an_error() {
        let mut ts = TypeSet::builtins();
        let entries = vec![("Orphan".to_string(), "NoSuchParent".to_string())];
        let e = ts.add_pack(&entries, "bad-pack").unwrap_err();
        assert!(e.to_string().contains("unresolvable parent types"));
        assert!(e.to_string().contains("Orphan (parent NoSuchParent)"));
    }

    #[test]
    fn invalid_names_rejected() {
        let mut ts = TypeSet::builtins();
        for bad in [
            "relaxedStructure",
            "Relaxed Structure",
            "Relaxed-Structure",
            "",
        ] {
            assert!(ts.add(bad, "System", "test").is_err(), "'{bad}' accepted");
        }
        for good in ["WidgetResult", "X9", "X"] {
            assert!(ts.add(good, "Result", "test").is_ok(), "'{good}' rejected");
        }
    }

    #[test]
    fn all_types_is_deterministic() {
        let ts_a = TypeSet::builtins();
        let ts_b = TypeSet::builtins();
        let a = ts_a.all_types();
        let b = ts_b.all_types();
        assert_eq!(a, b);
        assert_eq!(a[0], "Artifact");
        assert_eq!(a.len(), HIERARCHY.len() + 1);
    }
}
