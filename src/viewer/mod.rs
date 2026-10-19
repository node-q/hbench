//! `hbench view`: serves hbench's viewer SPA (viewer/, embedded from viewer/dist by build.rs) and its API
//! over a jobs or tasks folder. Response shapes, ordering, filters, sorting and error codes are kept stable for
//! the SPA.

mod compare;
mod http;
mod jobs;
mod jobs_write;
mod model;
mod summaries;
mod tasks;
#[cfg(test)]
mod tests;
mod util;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

// Submodules reach each other's items through these globs and their own `use super::*`.
use compare::*;
use http::*;
use jobs::*;
use model::*;
use summaries::*;
use util::*;

pub(crate) use http::{parse_port_range, serve};

/// Environment types the launcher offers (hbench runs docker only).
const ENVIRONMENTS: &[&str] = &["docker"];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Jobs,
    Tasks,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Jobs => "jobs",
            Mode::Tasks => "tasks",
        }
    }
}

struct LaunchedRun {
    child: std::process::Child,
    log_path: PathBuf,
}

pub struct App {
    pub folder: PathBuf,
    pub mode: Mode,
    runs: Mutex<HashMap<String, LaunchedRun>>,
}

impl App {
    pub fn new(folder: PathBuf, mode: Mode) -> App {
        App { folder, mode, runs: Mutex::new(HashMap::new()) }
    }
}
