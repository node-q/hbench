//! cortex-code (port of upstream's agents/installed/cortex_code.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'cortex-code' is not ported yet")
}
