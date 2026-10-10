//! swe-agent (port of upstream's agents/installed/swe_agent.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'swe-agent' is not ported yet")
}
