//! openclaw (port of upstream's agents/installed/openclaw.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'openclaw' is not ported yet")
}
