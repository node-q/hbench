//! kimi-code (port of upstream's agents/installed/kimi_code.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'kimi-code' is not ported yet")
}
