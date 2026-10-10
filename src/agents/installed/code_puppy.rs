//! code-puppy (port of upstream's agents/installed/code_puppy.py) — not ported yet.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'code-puppy' is not ported yet")
}
