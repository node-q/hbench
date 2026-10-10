//! antigravity-sdk (port of upstream's agents/installed/antigravity_sdk.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'antigravity-sdk' is not ported yet")
}
