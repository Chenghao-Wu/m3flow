//! Workflow compiler: WorkflowSpec → CompiledWorkflow (plan §36).
//!
//! Pipeline: parameter binding → stage shorthand expansion → subworkflow
//! inlining (recursive, static) → foreach expansion → reference resolution →
//! static type checking → dependency analysis → topo sort. "Compile time" is
//! run-submission time: workflow *parameters* are concrete then, so foreach
//! lists over params expand fully (plan §19).
//!
//! Reference scopes:
//!   ${inputs.<name>}            workflow input artifact
//!   ${params.<name>}            workflow parameter
//!   ${<step>.<output>}          artifact produced by an earlier step
//!   ${<step>.<output>.<field>}  field of the artifact's data/metadata payload
//!   ${<foreach_step>.<output>}  list of artifacts across all expansions
//!   ${<var>}                    foreach loop variable
//! A step may only reference steps declared before it (declaration order).

use crate::ir::{CompiledWorkflow, InputBinding, IrNode, OutputBinding};
use m3flow_core::canon;
use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::expr::{condition_references, find_references, Reference};
use m3flow_core::specs::{
    canonicalize_param, InputDecl, RetrySpec, StageSpec, StepSpec, WorkflowSpec,
};
use m3flow_registry::Registry;
use std::collections::{BTreeMap, BTreeSet};

const MAX_EXPANSION_DEPTH: usize = 8;
const MAX_FOREACH_ITEMS: usize = 256;

pub struct Compiler<'r> {
    registry: &'r Registry,
}

/// Compile-time view of one (sub)workflow being expanded.
struct Scope {
    inputs: BTreeMap<String, InputDecl>,
    params: serde_json::Map<String, serde_json::Value>,
    loop_vars: BTreeMap<String, serde_json::Value>,
    prefix: String,
    depth: usize,
}

impl<'r> Compiler<'r> {
    pub fn new(registry: &'r Registry) -> Self {
        Self { registry }
    }

    pub fn compile(
        &self,
        spec: &WorkflowSpec,
        param_overrides: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<CompiledWorkflow> {
        let params = bind_workflow_params(spec, param_overrides)?;
        let mut scope = Scope {
            inputs: spec.inputs.clone(),
            params: params.as_object().cloned().unwrap_or_default(),
            loop_vars: BTreeMap::new(),
            prefix: String::new(),
            depth: 0,
        };
        let mut nodes = Vec::new();
        let mut symbols: BTreeMap<String, OutputBinding> = BTreeMap::new();
        self.expand_steps(spec, &mut scope, &mut nodes, &mut symbols)?;
        let outputs = bind_outputs(spec, &symbols)?;
        let nodes = topo_sort(nodes)?;
        let spec_hash = canon::hash_json(&serde_json::json!({
            "name": spec.name,
            "version": spec.version,
            "spec": serde_json::to_value(spec).unwrap_or_default(),
            "params": params,
        }));
        Ok(CompiledWorkflow {
            name: spec.name.clone(),
            version: spec.version.clone(),
            spec_hash,
            nodes,
            outputs,
            inputs_decl: spec.inputs.clone(),
            params,
        })
    }

    // ------------------------------------------------------------ expansion

    fn expand_steps(
        &self,
        spec: &WorkflowSpec,
        scope: &mut Scope,
        nodes: &mut Vec<IrNode>,
        symbols: &mut BTreeMap<String, OutputBinding>,
    ) -> Result<()> {
        if scope.depth > MAX_EXPANSION_DEPTH {
            return Err(M3FlowError::workflow(
                format!(
                    "workflow '{}' exceeds max nesting depth {MAX_EXPANSION_DEPTH} (recursive workflow?)",
                    spec.name
                ),
                None,
            ));
        }
        // Stage shorthand expands first so explicit steps can reference
        // stage outputs (e.g. ${stage_21_npt.state}).
        if let Some(stages) = &spec.stages {
            self.expand_stages(stages, spec, scope, nodes, symbols)?;
        }
        for (step_id, step) in &spec.steps {
            self.expand_step(step_id, step, spec, scope, nodes, symbols)?;
        }
        Ok(())
    }

    fn expand_stages(
        &self,
        stages: &[StageSpec],
        spec: &WorkflowSpec,
        scope: &mut Scope,
        nodes: &mut Vec<IrNode>,
        symbols: &mut BTreeMap<String, OutputBinding>,
    ) -> Result<()> {
        let mut prev_step: Option<String> = None;
        for (i, stage) in stages.iter().enumerate() {
            let task = match stage.ensemble.as_str() {
                "minimize" => "energy_minimize",
                "nvt" => "run_nvt",
                "npt" => "run_npt",
                "nve" => "run_nve",
                other => {
                    return Err(M3FlowError::workflow(
                        format!("unknown stage ensemble '{other}'"),
                        None,
                    ))
                }
            };
            let step_id = stage
                .name
                .clone()
                .unwrap_or_else(|| format!("stage_{:02}_{}", i + 1, stage.ensemble));

            let mut inputs = BTreeMap::new();
            match &prev_step {
                Some(prev) => {
                    inputs.insert(
                        "state".to_string(),
                        serde_json::Value::String(format!("${{{prev}.state}}")),
                    );
                }
                None => {
                    if spec.inputs.contains_key("system") {
                        inputs.insert(
                            "system".to_string(),
                            serde_json::Value::String("${inputs.system}".to_string()),
                        );
                    } else if spec.inputs.contains_key("state") {
                        inputs.insert(
                            "state".to_string(),
                            serde_json::Value::String("${inputs.state}".to_string()),
                        );
                    } else {
                        return Err(M3FlowError::workflow(
                            format!(
                                "workflow '{}' uses stages but declares no system/state input",
                                spec.name
                            ),
                            None,
                        ));
                    }
                }
            }

            let mut parameters = BTreeMap::new();
            if let Some(t) = &stage.temperature {
                parameters.insert("temperature".into(), t.clone());
            }
            if let Some(t0) = &stage.temperature_start {
                parameters.insert("temperature".into(), t0.clone());
            }
            if let Some(t1) = &stage.temperature_end {
                parameters.insert("temperature_end".into(), t1.clone());
            }
            if let Some(p) = &stage.pressure {
                parameters.insert("pressure".into(), p.clone());
            }
            if let Some(d) = &stage.duration {
                parameters.insert("duration".into(), d.clone());
            }
            if let Some(dt) = &stage.timestep {
                parameters.insert("timestep".into(), dt.clone());
            }

            let synthetic = StepSpec {
                task: Some(task.to_string()),
                inputs,
                parameters,
                ..Default::default()
            };
            self.expand_step(&step_id, &synthetic, spec, scope, nodes, symbols)?;
            prev_step = Some(step_id);
        }
        Ok(())
    }

    fn expand_step(
        &self,
        step_id: &str,
        step: &StepSpec,
        spec: &WorkflowSpec,
        scope: &mut Scope,
        nodes: &mut Vec<IrNode>,
        symbols: &mut BTreeMap<String, OutputBinding>,
    ) -> Result<()> {
        if symbols.contains_key(step_id) {
            return Err(M3FlowError::workflow(
                format!("duplicate step id '{step_id}'"),
                Some(step_id.to_string()),
            ));
        }

        // ---- foreach: statically expand into indexed copies
        if let Some(foreach) = &step.foreach {
            let var = step.loop_var.clone().unwrap_or_else(|| "item".to_string());
            if scope.inputs.contains_key(&var) || spec.steps.contains_key(&var) {
                return Err(M3FlowError::workflow(
                    format!("foreach variable '{var}' collides with an input or step id"),
                    Some(step_id.to_string()),
                ));
            }
            let items = resolve_foreach_items(foreach, scope, step_id)?;
            if items.len() > MAX_FOREACH_ITEMS {
                return Err(M3FlowError::workflow(
                    format!(
                        "foreach in '{step_id}' expands to {} items (max {MAX_FOREACH_ITEMS})",
                        items.len()
                    ),
                    Some(step_id.to_string()),
                ));
            }
            for (i, item) in items.iter().enumerate() {
                let expanded_id = format!("{step_id}__{i}");
                let sub = StepSpec { foreach: None, loop_var: None, ..step.clone() };
                scope.loop_vars.insert(var.clone(), item.clone());
                self.expand_step(&expanded_id, &sub, spec, scope, nodes, symbols)?;
                scope.loop_vars.remove(&var);
            }
            // Collect view: ${step_id.<out>} gathers all expansions, in order.
            if let Some(task_ref) = &step.task {
                let task = self.registry.task(task_ref).map_err(|e| with_step(e, step_id))?;
                for (out, decl) in &task.outputs {
                    symbols.insert(
                        format!("{step_id}.{out}"),
                        OutputBinding {
                            binding: InputBinding::Collect {
                                base: format!("{}{}", scope.prefix, step_id),
                                output: out.clone(),
                            },
                            artifact_type: decl.artifact_type.clone(),
                        },
                    );
                }
            }
            symbols.insert(
                step_id.to_string(),
                OutputBinding {
                    binding: InputBinding::Collect {
                        base: format!("{}{}", scope.prefix, step_id),
                        output: String::new(),
                    },
                    artifact_type: String::new(),
                },
            );
            return Ok(());
        }

        // ---- subworkflow: inline recursively
        if let Some(wf_ref) = &step.workflow {
            return self.expand_subworkflow(step_id, wf_ref, step, scope, nodes, symbols);
        }

        // ---- plain task step
        let task_ref = step.task.clone().ok_or_else(|| {
            M3FlowError::workflow("step needs 'task' or 'workflow'".to_string(), Some(step_id.to_string()))
        })?;
        let task = self.registry.task(&task_ref).map_err(|e| with_step(e, step_id))?;
        let node_id = format!("{}{}", scope.prefix, step_id);

        let mut inputs = BTreeMap::new();
        let mut deps: BTreeSet<String> = BTreeSet::new();
        for (input_name, raw) in &step.inputs {
            let (binding, have_type) =
                self.resolve_input_binding(raw, scope, symbols, step_id, input_name)?;
            let decl = task.inputs.get(input_name).ok_or_else(|| {
                M3FlowError::workflow(
                    format!("task '{}' has no input named '{input_name}'", task.name),
                    Some(step_id.to_string()),
                )
            })?;
            let is_collect = matches!(binding, InputBinding::Collect { .. });
            if decl.many && !is_collect {
                return Err(M3FlowError::workflow(
                    format!(
                        "input '{input_name}' of task '{}' expects a fan-in list; reference a foreach step (got a single artifact)",
                        task.name
                    ),
                    Some(step_id.to_string()),
                ));
            }
            if !decl.many && is_collect {
                return Err(M3FlowError::workflow(
                    format!(
                        "input '{input_name}' of task '{}' takes a single artifact but is bound to a foreach list",
                        task.name
                    ),
                    Some(step_id.to_string()),
                ));
            }
            check_input_type(&task.name, input_name, decl, &have_type, step_id)?;
            match &binding {
                InputBinding::NodeOutput { node, .. } => {
                    deps.insert(node.clone());
                }
                InputBinding::Collect { base, .. } => {
                    for n in nodes.iter() {
                        if n.id.starts_with(&format!("{base}__")) {
                            deps.insert(n.id.clone());
                        }
                    }
                }
                InputBinding::WorkflowInput { .. } => {}
            }
            inputs.insert(input_name.clone(), binding);
        }
        for (input_name, decl) in &task.inputs {
            if decl.required && !inputs.contains_key(input_name) {
                return Err(M3FlowError::workflow(
                    format!(
                        "required input '{input_name}' of task '{}' is not bound",
                        task.name
                    ),
                    Some(step_id.to_string()),
                ));
            }
        }

        // params: substitute loop vars / ${params.x} now; leave artifact
        // references for runtime but record their dependency edges.
        let mut params = BTreeMap::new();
        for (pname, praw) in &step.parameters {
            let mut refs = Vec::new();
            find_references(praw, &mut refs);
            for r in &refs {
                if let Some(dep) = self.ref_dependency(r, scope, symbols, step_id)? {
                    deps.insert(dep);
                }
            }
            let resolved = self.substitute_scoped(praw, scope);
            params.insert(pname.clone(), resolved);
        }

        if let Some(cond) = &step.condition {
            for r in condition_references(cond)? {
                if let Some(dep) = self.ref_dependency(&r, scope, symbols, step_id)? {
                    deps.insert(dep);
                }
            }
        }

        let declared_outputs: BTreeMap<String, String> = task
            .outputs
            .iter()
            .map(|(n, o)| (n.clone(), o.artifact_type.clone()))
            .collect();

        let label = node_label(&task.name, &params);
        nodes.push(IrNode {
            id: node_id.clone(),
            task: task.qualified(),
            provider: step.provider.clone(),
            inputs,
            params,
            deps: deps.into_iter().collect(),
            condition: step.condition.clone(),
            retry: step.retry.clone().unwrap_or(RetrySpec { max_attempts: 1, on: vec![] }),
            resources: step.resources.clone().or_else(|| task.resources.clone()),
            declared_outputs: declared_outputs.clone(),
            label,
        });

        for (out_name, out_type) in &declared_outputs {
            symbols.insert(
                format!("{step_id}.{out_name}"),
                OutputBinding {
                    binding: InputBinding::NodeOutput {
                        node: node_id.clone(),
                        output: out_name.clone(),
                    },
                    artifact_type: out_type.clone(),
                },
            );
        }
        symbols.insert(
            step_id.to_string(),
            OutputBinding {
                binding: InputBinding::NodeOutput {
                    node: node_id,
                    output: String::new(),
                },
                artifact_type: String::new(),
            },
        );
        Ok(())
    }

    fn expand_subworkflow(
        &self,
        step_id: &str,
        wf_ref: &str,
        step: &StepSpec,
        scope: &mut Scope,
        nodes: &mut Vec<IrNode>,
        symbols: &mut BTreeMap<String, OutputBinding>,
    ) -> Result<()> {
        let child = self.registry.workflow(wf_ref).map_err(|e| with_step(e, step_id))?;

        let mut overrides = serde_json::Map::new();
        for (k, v) in &step.parameters {
            overrides.insert(k.clone(), self.substitute_scoped(v, scope));
        }
        let child_params = bind_workflow_params(child, &overrides)?;

        // Map child workflow inputs to parent bindings.
        let mut child_seed: BTreeMap<String, OutputBinding> = BTreeMap::new();
        for (iname, idecl) in &child.inputs {
            let raw = step.inputs.get(iname).ok_or_else(|| {
                M3FlowError::workflow(
                    format!("subworkflow '{}' input '{iname}' is not bound", child.name),
                    Some(step_id.to_string()),
                )
            })?;
            let (binding, have) =
                self.resolve_input_binding(raw, scope, symbols, step_id, iname)?;
            check_input_type(&child.name, iname, idecl, &have, step_id)?;
            if !idecl.required && raw.is_null() {
                continue;
            }
            child_seed.insert(
                format!("inputs.{iname}"),
                OutputBinding { binding, artifact_type: have },
            );
        }

        let mut child_scope = Scope {
            inputs: child.inputs.clone(),
            params: child_params.as_object().cloned().unwrap_or_default(),
            loop_vars: scope.loop_vars.clone(),
            prefix: format!("{}{}.", scope.prefix, step_id),
            depth: scope.depth + 1,
        };
        self.expand_steps(child, &mut child_scope, nodes, &mut child_seed)?;

        // Child outputs become visible as ${step_id.<out>} in the parent.
        let child_outputs = bind_outputs(child, &child_seed)?;
        for (oname, obind) in child_outputs {
            symbols.insert(format!("{step_id}.{oname}"), obind);
        }
        symbols.insert(
            step_id.to_string(),
            OutputBinding {
                binding: InputBinding::NodeOutput { node: String::new(), output: String::new() },
                artifact_type: String::new(),
            },
        );
        Ok(())
    }

    // ------------------------------------------------------------ references

    /// Resolve a step input value to (binding, static artifact type).
    fn resolve_input_binding(
        &self,
        raw: &serde_json::Value,
        scope: &Scope,
        symbols: &BTreeMap<String, OutputBinding>,
        step_id: &str,
        input_name: &str,
    ) -> Result<(InputBinding, String)> {
        let s = raw.as_str().ok_or_else(|| {
            M3FlowError::workflow(
                format!("input '{input_name}' must be a ${{...}} reference string"),
                Some(step_id.to_string()),
            )
        })?;
        let r = Reference::whole(s).ok_or_else(|| {
            M3FlowError::workflow(
                format!("input '{input_name}' must be a single ${{...}} reference, got '{s}'"),
                Some(step_id.to_string()),
            )
        })?;
        let first = r.path[0].as_str();
        if first == "inputs" {
            let name = r.path.get(1).ok_or_else(|| {
                M3FlowError::workflow("malformed inputs reference".to_string(), Some(step_id.to_string()))
            })?;
            let decl = scope.inputs.get(name).ok_or_else(|| {
                M3FlowError::workflow(
                    format!("unknown workflow input '{name}'"),
                    Some(step_id.to_string()),
                )
            })?;
            return Ok((
                InputBinding::WorkflowInput { name: name.clone() },
                decl.artifact_type.clone(),
            ));
        }
        if first == "params" {
            return Err(M3FlowError::workflow(
                "workflow parameters cannot be bound as artifact inputs".to_string(),
                Some(step_id.to_string()),
            ));
        }
        if r.path.len() < 2 {
            return Err(M3FlowError::workflow(
                format!("reference '{s}' needs an output name, e.g. ${{{first}.state}}"),
                Some(step_id.to_string()),
            ));
        }
        let key = format!("{}.{}", r.path[0], r.path[1]);
        match symbols.get(&key) {
            Some(ob) => Ok((ob.binding.clone(), ob.artifact_type.clone())),
            None => Err(M3FlowError::workflow(
                format!(
                    "unknown reference '{s}' (step '{first}' not defined yet — declaration order matters)"
                ),
                Some(step_id.to_string()),
            )),
        }
    }

    /// Dependency edge contributed by a reference inside params/conditions.
    fn ref_dependency(
        &self,
        r: &Reference,
        scope: &Scope,
        symbols: &BTreeMap<String, OutputBinding>,
        step_id: &str,
    ) -> Result<Option<String>> {
        let first = r.path[0].as_str();
        if first == "inputs" {
            let name = r.path.get(1).cloned().unwrap_or_default();
            if scope.inputs.contains_key(&name) {
                return Ok(None);
            }
            return Err(M3FlowError::workflow(
                format!("unknown workflow input '{name}'"),
                Some(step_id.to_string()),
            ));
        }
        if first == "params" {
            let name = r.path.get(1).cloned().unwrap_or_default();
            if scope.params.contains_key(&name) {
                return Ok(None);
            }
            return Err(M3FlowError::workflow(
                format!("unknown workflow parameter '{name}'"),
                Some(step_id.to_string()),
            ));
        }
        if scope.loop_vars.contains_key(first) {
            return Ok(None);
        }
        if symbols.contains_key(first) {
            // dep on the step itself; runtime resolves artifact data fields
            return Ok(Some(format!("{}{}", scope.prefix, first)));
        }
        Err(M3FlowError::workflow(
            format!("cannot resolve reference {}", r.display()),
            Some(step_id.to_string()),
        ))
    }

    /// Substitute loop vars and ${params.x} in parameter values. References
    /// to step outputs and workflow inputs are left for runtime resolution.
    fn substitute_scoped(&self, v: &serde_json::Value, scope: &Scope) -> serde_json::Value {
        match v {
            serde_json::Value::String(s) => {
                if let Some(r) = Reference::whole(s) {
                    let first = r.path[0].as_str();
                    if let Some(item) = scope.loop_vars.get(first) {
                        if r.path.len() == 1 {
                            return item.clone();
                        }
                        return r.path[1..]
                            .iter()
                            .fold(item.clone(), |acc, key| {
                                acc.get(key).cloned().unwrap_or(serde_json::Value::Null)
                            });
                    }
                    if first == "params" {
                        return scope
                            .params
                            .get(r.path.get(1).map(|s| s.as_str()).unwrap_or(""))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                    }
                    return v.clone();
                }
                let mut out = s.clone();
                let mut refs = Vec::new();
                find_references(v, &mut refs);
                for r in refs {
                    let first = r.path[0].as_str();
                    let replacement: Option<String> = if let Some(item) = scope.loop_vars.get(first)
                    {
                        Some(json_display(item))
                    } else if first == "params" {
                        scope
                            .params
                            .get(r.path.get(1).map(|s| s.as_str()).unwrap_or(""))
                            .map(json_display)
                    } else {
                        None
                    };
                    if let Some(rep) = replacement {
                        out = out.replace(&r.display(), &rep);
                    }
                }
                serde_json::Value::String(out)
            }
            serde_json::Value::Array(a) => serde_json::Value::Array(
                a.iter().map(|x| self.substitute_scoped(x, scope)).collect(),
            ),
            serde_json::Value::Object(m) => serde_json::Value::Object(
                m.iter()
                    .map(|(k, x)| (k.clone(), self.substitute_scoped(x, scope)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

// ---------------------------------------------------------------- helpers

pub fn bind_workflow_params(
    spec: &WorkflowSpec,
    overrides: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value> {
    let mut out = serde_json::Map::new();
    for (name, decl) in &spec.parameters {
        let raw = overrides.get(name).or(decl.default.as_ref());
        match raw {
            Some(v) => {
                out.insert(name.clone(), canonicalize_param(&spec.name, name, decl, v)?);
            }
            None => {
                if decl.required {
                    return Err(M3FlowError::schema(format!(
                        "workflow '{}' parameter '{name}' is required",
                        spec.name
                    )));
                }
            }
        }
    }
    for k in overrides.keys() {
        if !spec.parameters.contains_key(k) {
            return Err(M3FlowError::schema(format!(
                "unknown parameter '{k}' for workflow '{}'",
                spec.name
            )));
        }
    }
    Ok(serde_json::Value::Object(out))
}

fn resolve_foreach_items(
    foreach: &serde_json::Value,
    scope: &Scope,
    step_id: &str,
) -> Result<Vec<serde_json::Value>> {
    match foreach {
        serde_json::Value::Array(a) => Ok(a.clone()),
        serde_json::Value::String(s) => {
            let r = Reference::whole(s).ok_or_else(|| {
                M3FlowError::workflow(
                    "foreach must be a list or a single ${{...}} reference".to_string(),
                    Some(step_id.into()),
                )
            })?;
            let first = r.path[0].as_str();
            let val = if first == "params" {
                scope
                    .params
                    .get(r.path.get(1).map(|s| s.as_str()).unwrap_or(""))
                    .cloned()
            } else {
                scope.loop_vars.get(first).cloned()
            };
            match val {
                Some(serde_json::Value::Array(a)) => Ok(a),
                Some(other) => Err(M3FlowError::workflow(
                    format!("foreach reference resolved to non-list: {other}"),
                    Some(step_id.into()),
                )),
                None => Err(M3FlowError::workflow(
                    format!("cannot resolve foreach reference '{s}'"),
                    Some(step_id.into()),
                )),
            }
        }
        _ => Err(M3FlowError::workflow(
            "foreach must be a list or reference".to_string(),
            Some(step_id.into()),
        )),
    }
}

fn bind_outputs(
    spec: &WorkflowSpec,
    symbols: &BTreeMap<String, OutputBinding>,
) -> Result<BTreeMap<String, OutputBinding>> {
    let mut out = BTreeMap::new();
    for (oname, odecl) in &spec.outputs {
        let r = Reference::whole(&odecl.value).ok_or_else(|| {
            M3FlowError::workflow(
                format!("workflow output '{oname}' must be a ${{...}} reference"),
                None,
            )
        })?;
        let key = r.path.join(".");
        let bound = symbols.get(&key).ok_or_else(|| {
            M3FlowError::workflow(
                format!("workflow output '{oname}' references unknown '{key}'"),
                None,
            )
        })?;
        out.insert(
            oname.clone(),
            OutputBinding {
                binding: bound.binding.clone(),
                artifact_type: bound.artifact_type.clone(),
            },
        );
    }
    Ok(out)
}

fn check_input_type(
    owner: &str,
    input_name: &str,
    decl: &InputDecl,
    have: &str,
    step_id: &str,
) -> Result<()> {
    if m3flow_core::atypes::is_subtype(have, &decl.artifact_type) {
        Ok(())
    } else {
        Err(M3FlowError::Type {
            message: format!(
                "step '{step_id}': input '{input_name}' of '{owner}' expects {} but is bound to {}",
                decl.artifact_type, have
            ),
            expected: Some(decl.artifact_type.clone()),
            received: Some(have.to_string()),
        })
    }
}

fn topo_sort(nodes: Vec<IrNode>) -> Result<Vec<IrNode>> {
    let mut by_id: std::collections::HashMap<String, IrNode> =
        std::collections::HashMap::with_capacity(nodes.len());
    let mut order: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, n) in nodes.into_iter().enumerate() {
        order.insert(n.id.clone(), i);
        by_id.insert(n.id.clone(), n);
    }
    let mut indeg: std::collections::HashMap<String, usize> =
        by_id.keys().map(|k| (k.clone(), 0)).collect();
    let mut dependents: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for n in by_id.values() {
        for d in &n.deps {
            if !by_id.contains_key(d) {
                return Err(M3FlowError::workflow(
                    format!("node '{}' depends on unknown '{d}'", n.id),
                    Some(n.id.clone()),
                ));
            }
            *indeg.get_mut(&n.id).unwrap() += 1;
            dependents.entry(d.clone()).or_default().push(n.id.clone());
        }
    }
    let mut ready: Vec<String> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| id.clone())
        .collect();
    ready.sort_by_key(|id| order[id]);
    let mut sorted: Vec<String> = Vec::with_capacity(by_id.len());
    while let Some(id) = ready.first().cloned() {
        ready.remove(0);
        sorted.push(id.clone());
        if let Some(deps) = dependents.get(&id) {
            for dep in deps {
                let d = indeg.get_mut(dep).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(dep.clone());
                }
            }
            ready.sort_by_key(|x| order[x]);
        }
    }
    if sorted.len() != by_id.len() {
        let remaining: Vec<String> = by_id
            .keys()
            .filter(|k| !sorted.contains(k))
            .cloned()
            .collect();
        return Err(M3FlowError::workflow(
            format!("dependency cycle involving: {}", remaining.join(", ")),
            None,
        ));
    }
    Ok(sorted.into_iter().map(|id| by_id.remove(&id).unwrap()).collect())
}

fn node_label(task: &str, params: &BTreeMap<String, serde_json::Value>) -> String {
    let mut parts = vec![task.to_string()];
    for k in ["temperature", "pressure", "duration"] {
        if let Some(v) = params.get(k) {
            parts.push(json_display(v));
        }
    }
    parts.join(" ")
}

fn json_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(m) => {
            if let (Some(val), Some(unit)) = (m.get("value"), m.get("unit")) {
                format!("{} {}", json_display(val), json_display(unit))
            } else {
                serde_json::to_string(v).unwrap_or_default()
            }
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn with_step(e: M3FlowError, step_id: &str) -> M3FlowError {
    match e {
        M3FlowError::Workflow { message, step } => M3FlowError::Workflow {
            message,
            step: step.or_else(|| Some(step_id.to_string())),
        },
        M3FlowError::NotFound { message } => M3FlowError::Workflow {
            message,
            step: Some(step_id.to_string()),
        },
        other => other,
    }
}
