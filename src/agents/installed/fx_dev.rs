//! fx-dev (port of upstream's agents/installed/fx_dev.py): fx from the dev channel, with trace logging.
//! The runtime lives in `fx.rs` (`Fx { dev: true }`), as FxDev subclasses Fx in upstream.

use crate::agents::{AgentArgs, AgentBox};
use anyhow::Result;

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    Ok(Box::new(super::fx::Fx::new(args, true)?))
}
