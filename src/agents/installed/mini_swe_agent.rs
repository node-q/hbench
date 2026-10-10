//! mini-swe-agent (port of upstream's agents/installed/mini_swe_agent.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'mini-swe-agent' is not ported yet")
}
