use m3flow_registry::Registry;
use m3flow_runtime::compile::Compiler;

#[test]
fn debug_plain_chain() {
    let reg = Registry::with_builtins().unwrap();
    let text = r#"
schema: workflow/v1
name: t1
version: 1.0.0
inputs:
  system: {type: SimulationSystem, required: true}
parameters:
  temperature: {type: temperature, default: 300 K}
stages:
  - {ensemble: minimize, name: m, duration: 0 fs}
steps:
  nvt:
    task: run_nvt
    inputs: {state: "${m.state}"}
    parameters:
      temperature: "${params.temperature}"
      duration: 5 ps
  dens:
    task: compute_density
    inputs: {thermo: "${nvt.thermo}"}
outputs:
  result: {value: "${dens.result}"}
"#;
    let mut reg2 = reg;
    reg2.load_text(text, "test").unwrap();
    let spec = reg2.workflow("t1").unwrap().clone();
    let c = Compiler::new(&reg2).compile(&spec, &Default::default());
    match c {
        Ok(w) => {
            for n in &w.nodes {
                println!("node {} task {} deps {:?}", n.id, n.task, n.deps);
            }
        }
        Err(e) => panic!("compile failed: {e}"),
    }
}
