//! Typed models of the spec documents (task/v1, workflow/v1, system/v1).

use crate::error::{M3FlowError, Result};
use crate::units::{Dimension, Quantity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------- TaskSpec

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub schema: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub category: TaskCategory,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, InputDecl>,
    #[serde(default)]
    pub parameters: BTreeMap<String, ParamDecl>,
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputDecl>,
    #[serde(default)]
    pub requirements: Requirements,
    #[serde(default)]
    pub validation: Vec<String>,
    #[serde(default)]
    pub implementations: Vec<ImplDecl>,
    #[serde(default)]
    pub resources: Option<Resources>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCategory {
    Construction,
    Simulation,
    Sampling,
    Analysis,
    Validation,
    Utility,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputDecl {
    #[serde(rename = "type")]
    pub artifact_type: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub many: bool,
    #[serde(default)]
    pub description: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamDecl {
    #[serde(rename = "type")]
    pub param_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputDecl {
    #[serde(rename = "type")]
    pub artifact_type: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Requirements {
    #[serde(default)]
    pub ensemble: Vec<String>,
    #[serde(default)]
    pub observables: Vec<String>,
    #[serde(default)]
    pub varying: Vec<String>,
    #[serde(default)]
    pub needs_time_axis: Option<bool>,
    #[serde(default)]
    pub dynamics_sensitive: Option<bool>,
    #[serde(default)]
    pub resolution: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplDecl {
    pub provider: String,
    #[serde(default)]
    pub provider_version: Option<String>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default)]
    pub cpu: Option<u32>,
    #[serde(default)]
    pub gpu: Option<u32>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub walltime: Option<String>,
}

impl TaskSpec {
    pub fn from_json(v: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(v.clone())
            .map_err(|e| M3FlowError::schema(format!("task spec decode failed: {e}")))
    }

    pub fn qualified(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// Canonicalize a raw parameter value against its declaration.
    pub fn canonical_param(
        &self,
        name: &str,
        raw: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let decl = self
            .parameters
            .get(name)
            .ok_or_else(|| M3FlowError::Schema {
                message: format!("unknown parameter '{name}' for task '{}'", self.name),
                details: vec![format!(
                    "declared parameters: {}",
                    self.parameters
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )],
            })?;
        canonicalize_param(&self.name, name, decl, raw)
    }
}

/// Canonicalize a parameter value given a declaration (shared by tasks and workflows).
pub fn canonicalize_param(
    owner: &str,
    name: &str,
    decl: &ParamDecl,
    raw: &serde_json::Value,
) -> Result<serde_json::Value> {
    let ctx = || format!("parameter '{name}' of '{owner}'");
    if let Some(dim) = Dimension::parse(&decl.param_type) {
        return Ok(Quantity::parse_json(dim, raw)
            .map_err(|e| M3FlowError::schema(format!("{}: {e}", ctx())))?
            .canonical_json());
    }
    match decl.param_type.as_str() {
        "string" => raw
            .as_str()
            .map(|s| serde_json::Value::String(s.to_string()))
            .ok_or_else(|| M3FlowError::schema(format!("{}: expected string", ctx()))),
        "integer" => raw
            .as_i64()
            .map(serde_json::Value::from)
            .ok_or_else(|| M3FlowError::schema(format!("{}: expected integer", ctx()))),
        "number" => raw
            .as_f64()
            .map(serde_json::Value::from)
            .ok_or_else(|| M3FlowError::schema(format!("{}: expected number", ctx()))),
        "boolean" => raw
            .as_bool()
            .map(serde_json::Value::from)
            .ok_or_else(|| M3FlowError::schema(format!("{}: expected boolean", ctx()))),
        "enum" => {
            let s = raw
                .as_str()
                .ok_or_else(|| M3FlowError::schema(format!("{}: expected string enum", ctx())))?;
            if decl.values.iter().any(|v| v == s) {
                Ok(serde_json::Value::String(s.to_string()))
            } else {
                Err(M3FlowError::schema(format!(
                    "{}: '{s}' not in allowed values [{}]",
                    ctx(),
                    decl.values.join(", ")
                )))
            }
        }
        "string_list" => {
            let arr = raw.as_array().ok_or_else(|| {
                M3FlowError::schema(format!("{}: expected list of strings", ctx()))
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for x in arr {
                out.push(serde_json::Value::String(
                    x.as_str()
                        .ok_or_else(|| {
                            M3FlowError::schema(format!("{}: expected list of strings", ctx()))
                        })?
                        .to_string(),
                ));
            }
            Ok(serde_json::Value::Array(out))
        }
        "number_list" => {
            let arr = raw.as_array().ok_or_else(|| {
                M3FlowError::schema(format!("{}: expected list of numbers", ctx()))
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for x in arr {
                out.push(serde_json::Value::from(x.as_f64().ok_or_else(|| {
                    M3FlowError::schema(format!("{}: expected list of numbers", ctx()))
                })?));
            }
            Ok(serde_json::Value::Array(out))
        }
        "map" => Ok(raw.clone()),
        other => Err(M3FlowError::schema(format!(
            "{}: unknown parameter type '{other}'",
            ctx()
        ))),
    }
}

// ------------------------------------------------------------ WorkflowSpec

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSpec {
    pub schema: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub domain: Vec<String>,
    #[serde(default)]
    pub purpose: Vec<String>,
    #[serde(default)]
    pub applicability: Option<serde_json::Value>,
    #[serde(default)]
    pub references: Vec<serde_json::Value>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, InputDecl>,
    #[serde(default)]
    pub parameters: BTreeMap<String, ParamDecl>,
    /// Declaration order is semantic: a step may only reference steps
    /// declared before it.
    #[serde(default)]
    pub steps: indexmap::IndexMap<String, StepSpec>,
    #[serde(default)]
    pub stages: Option<Vec<StageSpec>>,
    #[serde(default)]
    pub outputs: BTreeMap<String, WorkflowOutputDecl>,
}

fn default_kind() -> String {
    "workflow".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepSpec {
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub parameters: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub foreach: Option<serde_json::Value>,
    #[serde(default, rename = "as")]
    pub loop_var: Option<String>,
    #[serde(default)]
    pub retry: Option<RetrySpec>,
    #[serde(default)]
    pub resources: Option<Resources>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrySpec {
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    #[serde(default)]
    pub on: Vec<String>,
}

fn default_attempts() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageSpec {
    pub ensemble: String,
    #[serde(default)]
    pub temperature: Option<serde_json::Value>,
    #[serde(default)]
    pub temperature_start: Option<serde_json::Value>,
    #[serde(default)]
    pub temperature_end: Option<serde_json::Value>,
    #[serde(default)]
    pub pressure: Option<serde_json::Value>,
    #[serde(default)]
    pub pressure_start: Option<serde_json::Value>,
    #[serde(default)]
    pub pressure_end: Option<serde_json::Value>,
    /// Barostat keyword style for npt stages: iso | aniso | tri | xyz
    /// (xyz = per-axis x/y/z control; see LAMMPS fix nh).
    #[serde(default)]
    pub pressure_style: Option<String>,
    /// Axis coupling for pressure_style = xyz: xyz | xy | xz | yz | none.
    #[serde(default)]
    pub couple: Option<String>,
    #[serde(default)]
    pub pressure_x: Option<serde_json::Value>,
    #[serde(default)]
    pub pressure_x_end: Option<serde_json::Value>,
    #[serde(default)]
    pub pressure_y: Option<serde_json::Value>,
    #[serde(default)]
    pub pressure_y_end: Option<serde_json::Value>,
    #[serde(default)]
    pub pressure_z: Option<serde_json::Value>,
    #[serde(default)]
    pub pressure_z_end: Option<serde_json::Value>,
    #[serde(default)]
    pub duration: Option<serde_json::Value>,
    #[serde(default)]
    pub timestep: Option<serde_json::Value>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowOutputDecl {
    pub value: String,
    #[serde(default)]
    pub description: String,
}

impl WorkflowSpec {
    pub fn from_json(v: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(v.clone())
            .map_err(|e| M3FlowError::schema(format!("workflow spec decode failed: {e}")))
    }

    pub fn qualified(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

// -------------------------------------------------------------- SystemSpec

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSpec {
    pub schema: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub components: Vec<Component>,
    pub environment: Environment,
    pub resolution: Resolution,
}

fn default_version() -> String {
    "1.0.0".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub id: String,
    #[serde(rename = "type")]
    pub component_type: String,
    pub representation: Representation,
    #[serde(default)]
    pub topology: Option<String>,
    #[serde(default)]
    pub tacticity: Option<String>,
    #[serde(default)]
    pub degree_of_polymerization: Option<u32>,
    #[serde(default)]
    pub number_of_chains: Option<u32>,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub force_field: Option<String>,
    #[serde(default)]
    pub charge_method: Option<String>,
    #[serde(default)]
    pub bead_spring: Option<serde_json::Value>,
    #[serde(default)]
    pub options: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Representation {
    #[serde(rename = "type")]
    pub repr_type: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub sequence: Option<Vec<String>>,
    #[serde(default)]
    pub first: Option<String>,
    #[serde(default)]
    pub middle: Option<String>,
    #[serde(default)]
    pub last: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    #[serde(rename = "type")]
    pub env_type: String,
    #[serde(default)]
    pub target_density: Option<serde_json::Value>,
    #[serde(default, rename = "box")]
    pub box_dims: Option<serde_json::Value>,
    #[serde(default)]
    pub normal: Option<String>,
    #[serde(default)]
    pub lower: Option<String>,
    #[serde(default)]
    pub upper: Option<String>,
    #[serde(default)]
    pub gap: Option<serde_json::Value>,
    #[serde(default)]
    pub temperature: Option<serde_json::Value>,
    #[serde(default)]
    pub options: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    #[serde(rename = "type")]
    pub resolution_type: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub force_field: Option<String>,
}

impl SystemSpec {
    pub fn from_json(v: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(v.clone())
            .map_err(|e| M3FlowError::schema(format!("system spec decode failed: {e}")))
    }
}

/// Parse a versioned reference `name@1.2.3` (version optional).
pub fn parse_ref(s: &str) -> (String, Option<String>) {
    match s.split_once('@') {
        Some((n, v)) => (n.to_string(), Some(v.to_string())),
        None => (s.to_string(), None),
    }
}
