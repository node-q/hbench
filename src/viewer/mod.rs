//! `hbench view`: serves hbench's viewer SPA (viewer/, embedded from viewer/dist by build.rs) and its API
//! over a jobs or tasks folder. Response shapes, ordering, filters, sorting and error codes are kept stable for
//! the SPA.

mod catalog;
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
    /// Creating this file asks the run to cancel (see [`crate::STOP_FILE_ENV`]).
    stop_file: PathBuf,
}

pub struct App {
    pub folder: PathBuf,
    pub mode: Mode,
    /// Write routes (starting runs, deleting jobs) answer only then: they run hbench with any config the
    /// request gives, so they must not be reachable from the network.
    bound_to_loopback: bool,
    runs: Mutex<HashMap<String, LaunchedRun>>,
    /// The dataset catalog as last listed.
    catalog_cache: Mutex<Option<catalog::Listed>>,
    /// The port it listens on: a request must name it in `Host` (see [`http::guard`]).
    port: u16,
    /// The secret made at each launch. Write routes and routes that use the sign-in need it in `X-Viewer-Key`.
    key: String,
}

impl App {
    pub fn new(folder: PathBuf, mode: Mode, bound_to_loopback: bool, port: u16, key: String) -> App {
        App { folder, mode, bound_to_loopback, runs: Mutex::new(HashMap::new()), catalog_cache: Mutex::new(None), port, key }
    }
}
