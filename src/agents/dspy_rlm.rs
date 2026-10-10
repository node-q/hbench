//! DSPy RLM agent (port of upstream's agents/dspy_rlm) — not ported yet.

use super::{AgentArgs, AgentBox};
use anyhow::{Result, bail};

pub fn create(_args: AgentArgs) -> Result<AgentBox> {
    bail!("agent 'dspy-rlm' is not ported yet")
}
