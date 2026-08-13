//! M3Flow execution cockpit TUI (ratatui). Full implementation lands in
//! Phase 8; this stub keeps the CLI surface stable.

use m3flow_core::error::Result;
use m3flow_runtime::project::Project;

pub fn run(_project: &Project, _run_id: Option<&str>) -> Result<()> {
    eprintln!("m3flow tui: not yet implemented (Phase 8)");
    Ok(())
}
