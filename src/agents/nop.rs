use super::{Agent, AgentContext, AgentCtx};
use anyhow::Result;

pub struct Nop;

#[async_trait::async_trait]
impl Agent for Nop {
    fn name(&self) -> &str {
        "nop"
    }
    fn version(&self) -> Option<String> {
        Some("1.0.0".into())
    }
    async fn setup(&mut self, _: &AgentCtx<'_>) -> Result<()> {
        Ok(())
    }
    async fn run(&mut self, _: &str, _: &AgentCtx<'_>, _: &mut AgentContext, _: bool) -> Result<()> {
        Ok(())
    }
}
