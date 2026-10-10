//! copilot-cli (port of upstream's agents/installed/copilot_cli.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'copilot-cli' is not ported yet")
}
