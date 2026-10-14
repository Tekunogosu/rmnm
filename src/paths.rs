//! Filesystem locations, resolved at runtime from the XDG base directories.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};

/// Tauri app identifier of the official launcher; names its data directory.
const OFFICIAL_LAUNCHER_ID: &str = "com.monstersandmemories.launcher";

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "rmnm").context("could not determine the home directory")
}

/// `$XDG_DATA_HOME/rmnm`, normally `~/.local/share/rmnm`.
pub fn data_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().to_path_buf())
}

pub fn database_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("rmnm.db"))
}

pub fn default_install_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("mnm"))
}

/// umu's own default prefix location for a GAMEID: `~/Games/umu/<game_id>`.
pub fn default_wine_prefix(game_id: &str) -> Result<PathBuf> {
    let base = BaseDirs::new().context("could not determine the home directory")?;
    Ok(base.home_dir().join("Games").join("umu").join(game_id))
}

/// Where the official launcher keeps `launcher.db`; it follows `$XDG_DATA_HOME` the same way.
pub fn official_launcher_db() -> Result<PathBuf> {
    let base = BaseDirs::new().context("could not determine the home directory")?;
    Ok(base
        .data_dir()
        .join(OFFICIAL_LAUNCHER_ID)
        .join("launcher.db"))
}
