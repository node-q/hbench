//! `hbench exec` (experimental): compile paths + an instruction into tasks, run a map
//! job over them, and optionally compile one reducer task over the map artifacts and run it.
//! `--print-config` output and compiled task dirs match the goldens in tests/fixtures/exec.

mod cli;
mod compiler;
mod executor;
mod flags;
mod mentions;
mod models;
mod paths;
mod print_config;
mod task_toml;
#[cfg(test)]
mod tests;
mod toml_writer;

// Submodules reach each other's items through these globs and their own `use super::*`.
use cli::*;
use compiler::*;
use executor::*;
use flags::*;
use mentions::*;
use models::*;
use paths::*;
use print_config::*;
use task_toml::*;
use toml_writer::*;

pub(crate) use cli::{ExecArgs, run};

const TPL_AUTO_VERIFY: &str = include_str!("../../assets/compile/compiled-task-template/tests/auto-verify-artifacts.sh");
const TPL_AUTO_VERIFY_SCHEMAS: &str =
    include_str!("../../assets/compile/compiled-task-template/tests/auto-verify-artifacts-with-schemas.sh");
const TPL_SCHEMA_VALIDATOR: &str = include_str!("../../assets/compile/compiled-task-template/tests/validate_artifact_schemas.py");
const TPL_PROMOTE_REWARD: &str = include_str!("../../assets/compile/compiled-task-template/tests/promote_reward_artifact.py");

const DEFAULT_IMAGE: &str = "ubuntu:latest";
const DEFAULT_WORKDIR: &str = "/app";
