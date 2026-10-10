//! Terminus-2: upstream's reference tmux-driven LLM agent (port in progress).

use super::{Agent, AgentArgs, AgentContext, AgentCtx};
use anyhow::{Result, bail};

pub struct Terminus2 {
    #[allow(dead_code)]
    args: AgentArgs,
}

impl Terminus2 {
    pub fn new(args: AgentArgs) -> Result<Terminus2> {
        Ok(Terminus2 { args })
    }
}

#[async_trait::async_trait]
impl Agent for Terminus2 {
    fn name(&self) -> &str {
        "terminus-2"
    }
    fn version(&self) -> Option<String> {
        Some("2.0.0".into())
    }
    async fn setup(&mut self, _: &AgentCtx<'_>) -> Result<()> {
        Ok(())
    }
    async fn run(&mut self, _: &str, _: &AgentCtx<'_>, _: &mut AgentContext, _: bool) -> Result<()> {
        bail!("terminus-2 is not implemented yet")
    }
}
