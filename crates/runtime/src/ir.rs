//! Workflow IR (plan §36): the compiled, statically-expanded node graph.

use m3flow_core::specs::{InputDecl, Resources, RetrySpec};
use serde::Serialize;
use std::collections::BTreeMap;

/// How a task input is wired after compilation.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputBinding {
    /// Workflow-level input artifact.
    WorkflowInput { name: String },
    /// Output `output` of node `node`.
    NodeOutput { node: String, output: String },
    /// All expansions of a foreach step's output (fan-in, preserves order).
    Collect { base: String, output: String },
}

impl InputBinding {
    pub fn dependency(&self) -> Vec<String> {
        match self {
            Self::WorkflowInput { .. } => Vec::new(),
            Self::NodeOutput { node, .. } => vec![node.clone()],
            Self::Collect { .. } => Vec::new(), // expansions add their own edges
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::WorkflowInput { name } => format!("${{inputs.{name}}}"),
            Self::NodeOutput { node, output } => format!("${{{node}.{output}}}"),
            Self::Collect { base, output } => format!("${{{base}[*].{output}}}"),
        }
    }
}

/// A workflow output mapped through (possibly nested) expansions.
#[derive(Debug, Clone, Serialize)]
pub struct OutputBinding {
    pub binding: InputBinding,
    pub artifact_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IrNode {
    pub id: String,
    /// Resolved task reference `name@version`.
    pub task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub inputs: BTreeMap<String, InputBinding>,
    /// Parameters possibly containing `${...}` reference strings; resolved
    /// against loop variables / workflow params / artifact data at runtime.
    pub params: BTreeMap<String, serde_json::Value>,
    pub deps: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    pub retry: RetrySpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    /// output name -> declared artifact type
    pub declared_outputs: BTreeMap<String, String>,
    /// Human-readable annotation, e.g. `nvt 300 K 50 ps`.
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompiledWorkflow {
    pub name: String,
    pub version: String,
    /// Fingerprint of spec + resolved parameters (plan §54).
    pub spec_hash: String,
    /// Topologically sorted (dependencies first).
    pub nodes: Vec<IrNode>,
    pub outputs: BTreeMap<String, OutputBinding>,
    pub inputs_decl: BTreeMap<String, InputDecl>,
    pub params: serde_json::Value,
}

impl CompiledWorkflow {
    pub fn node(&self, id: &str) -> Option<&IrNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn topo_index(&self) -> std::collections::HashMap<&str, usize> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect()
    }
}
