//! qwen-coder (port of upstream's agents/installed/qwen_code.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'qwen-coder' is not ported yet")
}
