//! Runs the task's reference solution (solution/solve.sh, or steps/<name>/solution/solve.sh).

use super::{Agent, AgentArgs, AgentContext, AgentCtx};
use crate::task::resolve_env_vars;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::time::Duration;

pub struct Oracle {
    args: AgentArgs,
}

impl Oracle {
    pub fn new(args: AgentArgs) -> Oracle {
        Oracle { args }
    }
}

#[async_trait::async_trait]
impl Agent for Oracle {
    fn name(&self) -> &str {
        "oracle"
    }
    fn version(&self) -> Option<String> {
        Some("1.0.0".into())
    }
    async fn setup(&mut self, _: &AgentCtx<'_>) -> Result<()> {
        Ok(())
    }

    async fn run(&mut self, _instruction: &str, cx: &AgentCtx<'_>, _ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        let task = &self.args.task;
        let step_sol = cx.step.as_ref().map(|s| task.step_dir(s).join("solution"));
        let sol = match step_sol {
            Some(d) if d.exists() => {
                if !d.join("solve.sh").exists() {
                    bail!("No OS-compatible solution script found for step: expected {}", d.join("solve.sh").display());
                }
                d
            }
            _ => task.solution_dir(),
        };
        if !sol.join("solve.sh").exists() {
            bail!("Solution script not found: {}", sol.join("solve.sh").display());
        }
        cx.env.empty_dirs(&["/solution"], false).await?;
        cx.env.upload_dir(&sol, "/solution").await?;
        cx.env.exec_root("chmod +x /solution/solve.sh", None).await?;
        // Task solution.env stays authoritative over the agent env overlay.
        let mut env: BTreeMap<String, String> = cx.overlay.clone();
        env.insert("DEBIAN_FRONTEND".into(), "noninteractive".into());
        env.extend(resolve_env_vars(&task.config.solution.env)?);
        let r = cx.env.exec("(/solution/solve.sh) > /logs/agent/oracle.txt 2>&1", cx.user.as_deref(), &env, None).await?;
        if crate::observe::enabled() {
            let _ = cx.env.exec_root("tail -n 200 /logs/agent/oracle.txt", Some(Duration::from_secs(60))).await;
        }
        if r.code != 0 {
            std::fs::create_dir_all(&cx.logs_dir)?;
            std::fs::write(cx.logs_dir.join("exit-code.txt"), r.code.to_string())?;
        }
        Ok(())
    }
}
