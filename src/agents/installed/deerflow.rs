//! deerflow (port of upstream's agents/installed/deerflow.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'deerflow' is not ported yet")
}
