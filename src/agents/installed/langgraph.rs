//! langgraph (port of upstream's agents/installed/langgraph.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'langgraph' is not ported yet")
}
