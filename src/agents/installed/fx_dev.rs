//! fx-dev (port of upstream's agents/installed/fx_dev.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'fx-dev' is not ported yet")
}
