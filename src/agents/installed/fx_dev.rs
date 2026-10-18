//! fx-dev: fx from the dev channel, with trace logging.
//! The runtime lives in `fx.rs` (`Fx { dev: true }`).

use crate::agents::{AgentArgs, AgentBox};
use anyhow::Result;

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(super::fx::Fx::new(args, true)?))
}
