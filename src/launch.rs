//! Starting the game through umu.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::db::EnvVar;
use crate::settings::{Setting, Settings};

/// Variables an AppImage launcher leaks into its children that break umu's system Python
/// ("No module named 'encodings'"). rmnm may itself be started from such an environment.
const LEAKED_APPIMAGE_VARS: [&str; 6] = [
    "PYTHONHOME",
    "PYTHONPATH",
    "PYTHONDONTWRITEBYTECODE",
    "LD_LIBRARY_PATH",
    "PERLLIB",
    "QT_PLUGIN_PATH",
];

/// Everything needed to start the game, resolved from the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: String,
    pub game_exe: PathBuf,
    pub game_args: Vec<String>,
    pub working_dir: PathBuf,
    pub wine_prefix: PathBuf,
    /// Variables set for the game, in the order they are applied (later ones win).
    pub env: Vec<(String, String)>,
}

impl LaunchPlan {
    pub fn new(settings: &Settings, env_vars: &[EnvVar]) -> Result<Self> {
        let install_dir = PathBuf::from(settings.get(Setting::InstallDir));
        let game_exe = install_dir.join("mnm.exe");
        if !game_exe.is_file() {
            bail!("{} is missing; run Patch first", game_exe.display());
        }
        let wine_prefix = PathBuf::from(settings.get(Setting::WinePrefix));
        if !wine_prefix.is_absolute() {
            bail!("MNM_WINEPREFIX must be an absolute path");
        }
        let mut env = vec![
            ("GAMEID".to_string(), settings.get(Setting::GameId)),
            ("WINEPREFIX".to_string(), wine_prefix.display().to_string()),
        ];
        let proton = settings.get(Setting::ProtonPath);
        if !proton.is_empty() {
            env.push(("PROTONPATH".to_string(), proton));
        }
        env.extend(
            env_vars
                .iter()
                .filter(|v| v.enabled)
                .map(|v| (v.name.clone(), v.value.clone())),
        );
        Ok(Self {
            program: settings.get(Setting::UmuCommand),
            game_exe,
            game_args: settings
                .get(Setting::GameArgs)
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            working_dir: install_dir,
            wine_prefix,
            env,
        })
    }

    /// The command to run. The token goes last, as `--token <token>`; the game reads it from
    /// its command line, which is the only way it accepts one.
    pub fn command(&self, token: &str) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg(&self.game_exe)
            .args(&self.game_args)
            .arg("--token")
            .arg(token)
            .current_dir(&self.working_dir);
        for var in LEAKED_APPIMAGE_VARS {
            command.env_remove(var);
        }
        command.envs(self.env.iter().map(|(k, v)| (k, v)));
        command
    }

    /// The command line as it can be shown or logged, with the token hidden.
    pub fn describe(&self) -> String {
        let mut parts = vec![self.program.clone(), self.game_exe.display().to_string()];
        parts.extend(self.game_args.iter().cloned());
        parts.push("--token <hidden>".into());
        parts.join(" ")
    }

    /// Starts the game in its own process group, with its output appended to `log_file`.
    pub fn spawn(&self, token: &str, log_file: &Path) -> Result<Child> {
        if let Some(parent) = self.wine_prefix.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let log = open_log(log_file)?;
        let mut header = log.try_clone()?;
        writeln!(header, "$ {}", self.describe())?;
        writeln!(header, "  in {}", self.working_dir.display())?;
        for (name, value) in &self.env {
            writeln!(header, "  {name}={value}")?;
        }
        let mut command = self.command(token);
        command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Keep terminal signals aimed at rmnm away from the game.
            .process_group(0);
        command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "{} was not found; install umu-launcher or set the umu-run command in Settings",
                    self.program
                )
            } else {
                anyhow::Error::from(e).context(format!("starting {}", self.program))
            }
        })
    }
}

/// Opens the game log, keeping the previous launch's log as `<name>.1`.
fn open_log(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if path.exists() {
        let mut previous = path.as_os_str().to_os_string();
        previous.push(".1");
        fs::rename(path, previous).with_context(|| format!("rotating {}", path.display()))?;
    }
    File::create(path).with_context(|| format!("creating {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("mnm.exe"), b"").unwrap();
        dir
    }

    fn var(name: &str, value: &str, enabled: bool) -> EnvVar {
        EnvVar {
            id: 0,
            name: name.into(),
            value: value.into(),
            enabled,
        }
    }

    #[test]
    fn builds_umu_command_from_settings() {
        let dir = install();
        let install_dir = dir.path().to_str().unwrap();
        let settings = Settings::with_overrides(&[
            (Setting::InstallDir, install_dir),
            (Setting::WinePrefix, "/srv/prefix"),
            (Setting::ProtonPath, "GE-Proton"),
            (Setting::GameArgs, "-force-d3d12  -popupwindow"),
        ]);
        let vars = [var("DXVK_HUD", "fps", true), var("MANGOHUD", "1", false)];
        let plan = LaunchPlan::new(&settings, &vars).unwrap();
        let command = plan.command("a.b.c");

        assert_eq!(command.get_program(), "umu-run");
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            args,
            [
                &format!("{install_dir}/mnm.exe"),
                "-force-d3d12",
                "-popupwindow",
                "--token",
                "a.b.c"
            ]
        );
        assert_eq!(command.get_current_dir(), Some(dir.path()));

        let envs: std::collections::HashMap<_, _> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_str().unwrap().to_string(),
                    v.map(|v| v.to_str().unwrap().to_string()),
                )
            })
            .collect();
        assert_eq!(envs["GAMEID"].as_deref(), Some("mnm"));
        assert_eq!(envs["WINEPREFIX"].as_deref(), Some("/srv/prefix"));
        assert_eq!(envs["PROTONPATH"].as_deref(), Some("GE-Proton"));
        assert_eq!(envs["DXVK_HUD"].as_deref(), Some("fps"));
        assert!(
            !envs.contains_key("MANGOHUD"),
            "disabled variables are not passed"
        );
        assert_eq!(
            envs["PYTHONHOME"], None,
            "leaked AppImage variables are removed"
        );
        assert!(!plan.describe().contains("a.b.c"));
    }

    #[test]
    fn user_variables_can_override_umu_ones() {
        let dir = install();
        let settings =
            Settings::with_overrides(&[(Setting::InstallDir, dir.path().to_str().unwrap())]);
        let plan = LaunchPlan::new(&settings, &[var("GAMEID", "umu-12345", true)]).unwrap();
        let last_gameid = plan.env.iter().rev().find(|(k, _)| k == "GAMEID").unwrap();
        assert_eq!(last_gameid.1, "umu-12345");
        assert!(
            !plan.env.iter().any(|(k, _)| k == "PROTONPATH"),
            "unset Proton is not passed"
        );
    }

    #[test]
    fn refuses_missing_game_or_relative_prefix() {
        let empty = tempfile::tempdir().unwrap();
        let settings =
            Settings::with_overrides(&[(Setting::InstallDir, empty.path().to_str().unwrap())]);
        assert!(LaunchPlan::new(&settings, &[]).is_err());

        let dir = install();
        let settings = Settings::with_overrides(&[
            (Setting::InstallDir, dir.path().to_str().unwrap()),
            (Setting::WinePrefix, "prefix"),
        ]);
        assert!(LaunchPlan::new(&settings, &[]).is_err());
    }

    #[test]
    fn log_rotation_keeps_one_previous_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/game.log");
        writeln!(open_log(&log).unwrap(), "first").unwrap();
        writeln!(open_log(&log).unwrap(), "second").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("logs/game.log.1")).unwrap(),
            "first\n"
        );
        assert_eq!(fs::read_to_string(&log).unwrap(), "second\n");
    }
}
