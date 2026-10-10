//! rovodev-cli (port of upstream's agents/installed/rovodev_cli.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'rovodev-cli' is not ported yet")
}
