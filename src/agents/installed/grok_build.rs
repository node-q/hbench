//! grok-build (port of upstream's agents/installed/grok_build.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'grok-build' is not ported yet")
}
