//! openhands-sdk (port of upstream's agents/installed/openhands_sdk.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'openhands-sdk' is not ported yet")
}
