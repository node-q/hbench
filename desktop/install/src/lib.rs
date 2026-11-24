//! The one-click install link (`actuaria://install/actuaria/<bot>`), kept apart from the Tauri app so it builds
//! and tests without the webview: the link parser, the prompt rate limit, the catalogue checks,
//! the download and its SHA-256, the safe unzip, and finding and calling the host's own installer. Also the run
//! link (`actuaria://run/actuaria/<dataset>`), which shares the parser's checks and the rate limit.
//!
//! Nothing that comes from the link is trusted beyond the bot id and the target: the host, the artefact URL and
//! its digest come from the catalogue.

pub mod catalogue;
pub mod download;
pub mod hosts;
pub mod link;
pub mod net;
pub mod rate;
pub mod register;
pub mod run;
pub mod unzip;

pub use link::{Link, Target};
