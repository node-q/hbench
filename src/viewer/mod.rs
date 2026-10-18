//! `hbench view`: serves hbench's viewer SPA (viewer/, embedded from viewer/dist by build.rs) and its API
//! over a jobs or tasks folder. Response shapes, ordering, filters, sorting and error codes are kept stable for
//! the SPA; the hosted hub (auth/upload) and the `--stream` editor need cloud services, so they answer as
//! when signed out / unavailable.

mod compare;
mod http;
mod jobs;
mod jobs_write;
mod model;
mod py;
mod summaries;
mod tasks;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

// Submodules reach each other's items through these globs and their own `use super::*`.
use compare::*;
use http::*;
use jobs::*;
use model::*;
use py::*;
use summaries::*;

pub(crate) use http::{parse_port_range, serve};

const STREAM_HANDLE_NAME: &str = "stream.json";
const NOT_AUTHENTICATED: &str = "Not authenticated: hub sign-in is not supported by hbench.";
/// `EnvironmentType` values.
const ENVIRONMENTS: &[&str] = &[
    "docker",
    "podman",
    "daytona",
    "e2b",
    "modal",
    "runloop",
    "langsmith",
    "ec2",
    "gke",
    "ack",
    "openshift",
    "novita",
    "apple-container",
    "singularity",
    "islo",
    "tensorlake",
    "cwsandbox",
    "use-computer",
    "cua-cloud",
    "blaxel",
    "opensandbox",
    "beam",
    "skypilot",
    "hf-sandbox",
    "hyperbrowser",
    "vercel",
    "runta",
    "prime",
    "kata",
    "mosaic",
    "smol",
];

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
