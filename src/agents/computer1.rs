//! Computer-1 (computer-use agent) (port of upstream's agents/computer_1) — not ported yet.

use super::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'computer-1' is not ported yet")
}
