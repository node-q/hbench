//! strands (port of upstream's agents/installed/strands.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'strands' is not ported yet")
}
