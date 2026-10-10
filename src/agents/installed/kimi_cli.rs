//! kimi-cli (port of upstream's agents/installed/kimi_cli.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'kimi-cli' is not ported yet")
}
