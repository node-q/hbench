//! DSPy RLM agent.
//!
//! `dspy.RLM` runs host-side in Python: the host `python3` needs `dspy` (`pip install dspy`)
//! and Deno for dspy's interpreter sandbox. The embedded runner (assets/agents/dspy_rlm/rlm_runner.py)
//! exposes the environment tools to RLM; each tool call comes back to hbench over stdio and runs in
//! the environment with `docker compose exec`.

use super::terminus2::{Kw, mcp_info, quote_repr};
use super::{Agent, AgentArgs, AgentBox, AgentContext, AgentCtx};
use crate::env::{ExecResult, kind_err};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const RUNNER: &str = include_str!("../../assets/agents/dspy_rlm/rlm_runner.py");
const NAME: &str = "dspy-rlm";
const PYTHON: &str = "python3";

#[derive(Debug, Clone)]
struct Options {
    signature: String,
    max_iterations: i64,
    max_llm_calls: i64,
    max_output_chars: i64,
    verbose: bool,
    tool_timeout_sec: i64,
    working_dir: String,
    sub_model_name: Option<String>,
}

impl Options {
    fn parse(kwargs: &BTreeMap<String, Value>) -> Result<Options> {
        let mut kw = Kw::new(kwargs);
        let o = Options {
            signature: kw.string("signature").unwrap_or_else(|| "instruction, file_tree -> solution".into()),
            max_iterations: kw.int("max_iterations").unwrap_or(20),
            max_llm_calls: kw.int("max_llm_calls").unwrap_or(50),
            max_output_chars: kw.int("max_output_chars").unwrap_or(10_000),
            verbose: kw.boolean("verbose", false),
            tool_timeout_sec: kw.int("tool_timeout_sec").unwrap_or(30),
            working_dir: kw.string("working_dir").unwrap_or_else(|| "/".into()),
            sub_model_name: kw.string("sub_model_name"),
        };
        kw.finish(NAME)?;
        if kwargs.get("extra_tools").is_some_and(|v| !v.is_null()) {
            bail!("dspy-rlm extra_tools are Python callables, which can't be set from an hbench config");
        }
        Ok(o)
    }
}

/// Map (instruction, file_tree) onto the signature's two input fields, by position.
fn input_kwargs(signature: &str, instruction: &str, file_tree: &str) -> Result<Map<String, Value>> {
    let inputs = signature.split_once("->").map_or(signature, |(i, _)| i);
    let fields: Vec<&str> =
        inputs.split(',').filter(|f| !f.trim().is_empty()).map(|f| f.trim().split(':').next().unwrap_or("").trim()).collect();
    if fields.len() != 2 {
        bail!(
            "dspy-rlm signature must define exactly two input fields (instruction + file_tree semantics), got: {}",
            quote_repr(signature)
        );
    }
    Ok(Map::from_iter([(fields[0].to_string(), json!(instruction)), (fields[1].to_string(), json!(file_tree))]))
}

/// Environment execution for the tool bridge (`AgentCtx`; mocked in tests).
#[async_trait::async_trait]
trait Exec: Send + Sync {
    async fn exec_in(&self, cmd: &str, cwd: &str, timeout: Duration) -> Result<ExecResult>;
}

#[async_trait::async_trait]
impl Exec for AgentCtx<'_> {
    async fn exec_in(&self, cmd: &str, cwd: &str, timeout: Duration) -> Result<ExecResult> {
        self.env.exec_in(cmd, self.user.as_deref(), &self.overlay, Some(timeout), Some(cwd)).await
    }
}

/// Run the Python runner: send `cfg`, serve its exec requests, return its final message.
async fn run_runner(cfg: &Value, env: &BTreeMap<String, String>, log: &Path, ex: &dyn Exec) -> Result<Value> {
    if let Some(p) = log.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut child = tokio::process::Command::new(PYTHON)
        .args(["-u", "-c", RUNNER])
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(log)?)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running `{PYTHON}`: dspy-rlm runs dspy.RLM in the host Python"))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    stdin.write_all(format!("{cfg}\n").as_bytes()).await?;
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        match msg["op"].as_str() {
            Some("exec") => {
                let timeout = Duration::from_secs(msg["timeout_sec"].as_u64().unwrap_or(30));
                let r = ex.exec_in(msg["command"].as_str().unwrap_or(""), msg["cwd"].as_str().unwrap_or("/"), timeout).await;
                let resp = match r {
                    Ok(r) => json!({"return_code": r.code, "stdout": r.stdout, "stderr": r.stderr}),
                    Err(e) => json!({"error": format!("{e:#}")}),
                };
                stdin.write_all(format!("{resp}\n").as_bytes()).await?;
            }
            Some("done") => {
                drop(stdin);
                let _ = child.wait().await;
                return Ok(msg);
            }
            _ => {}
        }
    }
    let status = child.wait().await?;
    bail!("dspy-rlm runner exited ({status}) without a result; see {}", log.display())
}

pub struct DspyRlm {
    args: AgentArgs,
    opts: Options,
    version: Option<String>,
}

pub fn create(args: AgentArgs) -> Result<AgentBox> {
    let opts = Options::parse(&args.kwargs)?;
    Ok(Box::new(DspyRlm { args, opts, version: None }))
}

impl DspyRlm {
    /// Host env for the runner: dspy reads API keys from it (agent env overrides).
    fn host_env(&self) -> BTreeMap<String, String> {
        self.args.extra_env.clone()
    }

    fn apply(ctx: &mut AgentContext, done: &Value) {
        if let Some(n) = done["n_input_tokens"].as_i64() {
            ctx.n_input_tokens = Some(n);
        }
        if let Some(n) = done["n_output_tokens"].as_i64() {
            ctx.n_output_tokens = Some(n);
        }
        if let Some(c) = done["cost_usd"].as_f64() {
            ctx.cost_usd = Some(c);
        }
        if let Some(n) = done["rlm_trajectory_steps"].as_i64() {
            let md = ctx.metadata.get_or_insert_with(|| json!({}));
            md["rlm_trajectory_steps"] = json!(n);
        }
    }

    async fn run_with(&mut self, instruction: &str, ex: &dyn Exec, ctx: &mut AgentContext) -> Result<()> {
        let model = self.args.model_name.clone().ok_or_else(|| anyhow!("model_name is required for dspy-rlm"))?;
        let tree = ex.exec_in("find . -maxdepth 3 -type f | head -200", &self.opts.working_dir, Duration::from_secs(15)).await?;
        let file_tree = if tree.stdout.is_empty() { "(empty)".to_string() } else { tree.stdout };
        let instruction = format!("{instruction}{}", mcp_info(&self.args.mcp_servers));
        let o = &self.opts;
        let cfg = json!({
            "model_name": model, "sub_model_name": o.sub_model_name, "signature": o.signature,
            "max_iterations": o.max_iterations, "max_llm_calls": o.max_llm_calls, "max_output_chars": o.max_output_chars,
            "verbose": o.verbose, "tool_timeout_sec": o.tool_timeout_sec, "working_dir": o.working_dir,
            "input_kwargs": input_kwargs(&o.signature, &instruction, &file_tree)?, "logs_dir": self.args.logs_dir,
        });
        let done = run_runner(&cfg, &self.host_env(), &self.args.logs_dir.join("rlm").join("runner.log"), ex).await?;
        Self::apply(ctx, &done);
        match done["error"].as_str() {
            // The runner's exception type name becomes the trial's exception type.
            // Leaks the (short) class name once per failed run; kind_err wants &'static str.
            Some(e) => Err(kind_err(done["kind"].as_str().unwrap_or("RuntimeError"), e.to_string())),
            None => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl Agent for DspyRlm {
    fn name(&self) -> &str {
        NAME
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    /// No container-side setup (RLM runs host-side); read dspy's version from the host Python.
    async fn setup(&mut self, cx: &AgentCtx<'_>) -> Result<()> {
        let log = std::env::temp_dir().join(format!("hbench-dspy-version-{}.log", uuid::Uuid::new_v4().simple()));
        if let Ok(v) = run_runner(&json!({"op": "version"}), &self.host_env(), &log, cx).await {
            self.version = v["version"].as_str().map(String::from);
        }
        let _ = std::fs::remove_file(log);
        Ok(())
    }
    async fn run(&mut self, instruction: &str, cx: &AgentCtx<'_>, ctx: &mut AgentContext, _resume: bool) -> Result<()> {
        self.run_with(instruction, cx, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn signature_mapping_and_options() {
        let k = input_kwargs("instruction, file_tree -> solution", "Solve it", "./main.py").unwrap();
        assert_eq!(Value::Object(k), json!({"instruction": "Solve it", "file_tree": "./main.py"}));
        let k = input_kwargs("task: str, tree: str -> answer", "Solve it", "./main.py").unwrap();
        assert_eq!(Value::Object(k), json!({"task": "Solve it", "tree": "./main.py"}));
        let e = input_kwargs("question -> answer", "a", "b").unwrap_err().to_string();
        assert!(e.contains("exactly two input fields") && e.contains("'question -> answer'"));
        let o = Options::parse(
            &serde_json::from_value(json!({"signature": "context, question -> answer", "max_iterations": 10, "working_dir": "/workspace"}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!((o.max_iterations, o.max_llm_calls, o.working_dir.as_str()), (10, 50, "/workspace"));
        assert!(Options::parse(&serde_json::from_value(json!({"extra_tools": ["x"]})).unwrap()).is_err());
    }

    struct MockExec(Arc<Mutex<Vec<(String, String)>>>);
    #[async_trait::async_trait]
    impl Exec for MockExec {
        async fn exec_in(&self, cmd: &str, cwd: &str, _: Duration) -> Result<ExecResult> {
            self.0.lock().unwrap().push((cmd.into(), cwd.into()));
            let r = |code, out: &str, err: &str| Ok(ExecResult { code, stdout: out.into(), stderr: err.into() });
            match cmd {
                "echo hi" => r(0, "hi\n", ""),
                c if c.starts_with("find . -maxdepth 3") => r(0, "./a.py\n", ""),
                c if c.starts_with("cat ") => r(1, "", "No such file"),
                _ => r(0, "", ""),
            }
        }
    }

    /// Drives the real runner against a fake `dspy` package: the tool bridge, logs and usage.
    #[tokio::test]
    async fn runner_bridges_tools_with_fake_dspy() {
        if std::process::Command::new(PYTHON).arg("--version").output().is_err() {
            return; // no host Python
        }
        let root = std::env::temp_dir().join(format!("hbench-dspy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("pkg/dspy")).unwrap();
        std::fs::write(
            root.join("pkg/dspy/__init__.py"),
            r#"
__version__ = "9.9.9"
class LM:
    def __init__(self, model, max_tokens=None):
        self.model = model
        self.history = [{"cost": 0.002}, {"cost": 0.003}]
def configure(**kw):
    assert kw["track_usage"] is True
class Prediction(dict):
    def get_lm_usage(self):
        return {"m": {"input_tokens": 1000, "output_tokens": 200}, "s": {"input_tokens": 500, "output_tokens": 100}}
class RLM:
    def __init__(self, signature, tools, **kw):
        assert len(tools) == 7 and all(t.__doc__ for t in tools)
        self.tools = {t.__name__: t for t in tools}
    def __call__(self, **inputs):
        print("verbose chatter on stdout must not break the protocol")
        p = Prediction()
        p["solution"] = self.tools["exec_command"]("echo hi") + "|" + self.tools["read_file"]("/missing") + "|" + inputs["file_tree"]
        p.trajectory = [{"step": 1}, {"step": 2}]
        p.final_reasoning = "line 42"
        return p
"#,
        )
        .unwrap();
        let logs = root.join("trial/agent");
        let kwargs = serde_json::from_value(json!({"working_dir": "/app"})).unwrap();
        let task_dir = root.join("task");
        std::fs::create_dir_all(task_dir.join("environment")).unwrap();
        std::fs::write(task_dir.join("task.toml"), "[environment]\ndocker_image='x'\n").unwrap();
        std::fs::write(task_dir.join("instruction.md"), "x").unwrap();
        let args = AgentArgs {
            load_trajectory: None,
            model_name: Some("openai/gpt-4o".into()),
            kwargs,
            extra_env: BTreeMap::from([("PYTHONPATH".to_string(), root.join("pkg").display().to_string())]),
            mcp_servers: vec![],
            skills: vec![],
            logs_dir: logs.clone(),
            task: Arc::new(crate::task::Task::load(&task_dir).unwrap()),
        };
        let mut a = DspyRlm { opts: Options::parse(&args.kwargs).unwrap(), args, version: None };
        let calls = Arc::new(Mutex::new(vec![]));
        let mut ctx = AgentContext::default();
        a.run_with("Fix the bug", &MockExec(calls.clone()), &mut ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(logs.join("rlm/solution.txt")).unwrap(), "hi\n|[error] No such file|./a.py\n");
        assert_eq!(std::fs::read_to_string(logs.join("rlm/final_reasoning.txt")).unwrap(), "line 42");
        assert_eq!((ctx.n_input_tokens, ctx.n_output_tokens), (Some(1500), Some(300)));
        assert!((ctx.cost_usd.unwrap() - 0.005).abs() < 1e-12);
        assert_eq!(ctx.metadata.unwrap()["rlm_trajectory_steps"], 2);
        let calls = calls.lock().unwrap();
        assert_eq!(calls[1], ("echo hi".to_string(), "/app".to_string()));
        assert_eq!(calls[2].0, "cat /missing");
    }
}
