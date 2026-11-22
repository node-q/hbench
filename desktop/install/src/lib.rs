//! The one-click install link (`actuaria://install/actuaria/<bot>`), kept apart from the Tauri app so it builds
//! and tests without the webview: the link parser, the prompt rate limit, the catalogue and `bots.lock` checks,
//! the download and its SHA-256, the safe unzip, and finding and calling the host's own installer.
//!
//! Nothing that comes from the link is trusted beyond the bot id and the target: the host, the artefact URL and
//! its digest come from the catalogue, and the digest must also be in `bots.lock`.

pub mod catalogue;
pub mod download;
pub mod hosts;
pub mod link;
pub mod net;
pub mod rate;
pub mod register;
pub mod unzip;

pub use link::{Link, Target};
