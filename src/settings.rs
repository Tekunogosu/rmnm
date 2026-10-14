//! User-editable launcher settings. Only overrides are stored; an unset key means "use the default".

use std::collections::HashMap;

use anyhow::Result;

use crate::db::Db;
use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Setting {
    InstallDir,
    WinePrefix,
    ProtonPath,
    GameId,
    GameArgs,
    UmuCommand,
    OfficialDb,
}

impl Setting {
    pub const ALL: [Setting; 7] = [
        Setting::InstallDir,
        Setting::WinePrefix,
        Setting::ProtonPath,
        Setting::GameId,
        Setting::GameArgs,
        Setting::UmuCommand,
        Setting::OfficialDb,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::InstallDir => "install_dir",
            Self::WinePrefix => "wine_prefix",
            Self::ProtonPath => "proton_path",
            Self::GameId => "game_id",
            Self::GameArgs => "game_args",
            Self::UmuCommand => "umu_command",
            Self::OfficialDb => "official_db",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::InstallDir => "Install folder",
            Self::WinePrefix => "MNM_WINEPREFIX",
            Self::ProtonPath => "MNM_PROTONPATH",
            Self::GameId => "GAMEID",
            Self::GameArgs => "Game arguments",
            Self::UmuCommand => "umu-run command",
            Self::OfficialDb => "Official launcher DB",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Self::InstallDir => {
                "Folder holding the game files (mnm.exe). Patch and Repair write here."
            }
            Self::WinePrefix => {
                "Wine prefix umu creates and runs the game in. Passed to umu as WINEPREFIX."
            }
            Self::ProtonPath => {
                "Proton build for umu, e.g. GE-Proton or a full path. Empty uses UMU-Proton. Passed as PROTONPATH."
            }
            Self::GameId => "umu GAMEID; also names the default prefix folder.",
            Self::GameArgs => {
                "Extra arguments for mnm.exe, separated by spaces (e.g. -force-d3d12)."
            }
            Self::UmuCommand => {
                "Program that runs the game: umu-run from $PATH, or a full path to umu-run or a wrapper."
            }
            Self::OfficialDb => {
                "launcher.db to import a token from. Default follows $XDG_DATA_HOME."
            }
        }
    }
}

/// Snapshot of the stored overrides, with defaults resolved on read.
#[derive(Debug, Clone)]
pub struct Settings {
    overrides: HashMap<Setting, String>,
}

impl Settings {
    pub fn load(db: &Db) -> Result<Self> {
        let stored: HashMap<String, String> = db.all_settings()?.into_iter().collect();
        let overrides = Setting::ALL
            .into_iter()
            .filter_map(|s| stored.get(s.key()).map(|v| (s, v.clone())))
            .collect();
        Ok(Self { overrides })
    }

    /// Stores `value` as an override. A blank value, or one equal to the current default,
    /// clears the override so the setting keeps following its default.
    pub fn set(&self, db: &Db, setting: Setting, value: &str) -> Result<()> {
        let value = value.trim();
        if value.is_empty() || value == self.default_for(setting) {
            db.clear_setting(setting.key())
        } else {
            db.set_setting(setting.key(), value)
        }
    }

    #[cfg(test)]
    pub fn with_overrides(overrides: &[(Setting, &str)]) -> Self {
        Self {
            overrides: overrides.iter().map(|(s, v)| (*s, v.to_string())).collect(),
        }
    }

    pub fn reset(db: &Db, setting: Setting) -> Result<()> {
        db.clear_setting(setting.key())
    }

    pub fn is_default(&self, setting: Setting) -> bool {
        !self.overrides.contains_key(&setting)
    }

    /// The stored override, or the default when none is set. Empty means "no value".
    pub fn get(&self, setting: Setting) -> String {
        match self.overrides.get(&setting) {
            Some(value) => value.clone(),
            None => self.default_for(setting),
        }
    }

    fn default_for(&self, setting: Setting) -> String {
        let shown = |path: Result<std::path::PathBuf>| {
            path.map(|p| p.display().to_string()).unwrap_or_default()
        };
        match setting {
            Setting::InstallDir => shown(paths::default_install_dir()),
            Setting::WinePrefix => shown(paths::default_wine_prefix(&self.get(Setting::GameId))),
            Setting::ProtonPath | Setting::GameArgs => String::new(),
            Setting::GameId => "mnm".to_string(),
            Setting::UmuCommand => "umu-run".to_string(),
            Setting::OfficialDb => shown(paths::official_launcher_db()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_default_follows_game_id_until_overridden() {
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&db).unwrap();
        assert!(
            settings
                .get(Setting::WinePrefix)
                .ends_with("/Games/umu/mnm")
        );

        settings.set(&db, Setting::GameId, "mnm-test").unwrap();
        let settings = Settings::load(&db).unwrap();
        assert!(
            settings
                .get(Setting::WinePrefix)
                .ends_with("/Games/umu/mnm-test")
        );

        settings
            .set(&db, Setting::WinePrefix, "/srv/prefix")
            .unwrap();
        let settings = Settings::load(&db).unwrap();
        assert_eq!(settings.get(Setting::WinePrefix), "/srv/prefix");
        assert!(!settings.is_default(Setting::WinePrefix));
    }

    #[test]
    fn blank_value_resets_to_default() {
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&db).unwrap();
        settings.set(&db, Setting::ProtonPath, "GE-Proton").unwrap();
        settings.set(&db, Setting::ProtonPath, "   ").unwrap();
        assert!(Settings::load(&db).unwrap().is_default(Setting::ProtonPath));
    }

    #[test]
    fn saving_the_default_unchanged_keeps_following_it() {
        let db = Db::open_in_memory().unwrap();
        let settings = Settings::load(&db).unwrap();
        let default_prefix = settings.get(Setting::WinePrefix);
        settings
            .set(&db, Setting::WinePrefix, &default_prefix)
            .unwrap();
        assert!(Settings::load(&db).unwrap().is_default(Setting::WinePrefix));
    }
}
