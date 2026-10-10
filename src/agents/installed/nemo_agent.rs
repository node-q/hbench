//! nemo-agent (port of upstream's agents/installed/nemo_agent.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'nemo-agent' is not ported yet")
}
