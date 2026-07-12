use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const SETTINGS_VERSION: u32 = 1;

/// User-level preferences that affect only the interactive terminal interface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TuiSettings {
    version: u32,
    pub(crate) auto_diagnose_on_startup: bool,
    pub(crate) hide_unsupported_and_missing_tools: bool,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            auto_diagnose_on_startup: false,
            hide_unsupported_and_missing_tools: false,
        }
    }
}

impl TuiSettings {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let settings: Self = toml::from_str(&fs::read_to_string(path)?)?;
        if settings.version != SETTINGS_VERSION {
            return Err(Error::InvalidConfig(format!(
                "unsupported TUI settings version {}; expected {SETTINGS_VERSION}",
                settings.version
            )));
        }
        Ok(settings)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, toml::to_string_pretty(self)?)?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(temporary, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_manual_diagnostics_and_round_trip() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");

        let missing = TuiSettings::load(&path).expect("default settings");
        assert!(!missing.auto_diagnose_on_startup);
        assert!(!missing.hide_unsupported_and_missing_tools);

        let enabled = TuiSettings {
            auto_diagnose_on_startup: true,
            hide_unsupported_and_missing_tools: true,
            ..TuiSettings::default()
        };
        enabled.save(&path).expect("save settings");

        assert_eq!(TuiSettings::load(&path).expect("reload settings"), enabled);
        let serialized = fs::read_to_string(path).expect("read settings file");
        assert!(serialized.contains("auto_diagnose_on_startup = true"));
        assert!(serialized.contains("hide_unsupported_and_missing_tools = true"));
    }

    #[test]
    fn settings_reject_unknown_fields_and_versions() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");

        fs::write(&path, "version = 1\nunknown = true\n").expect("unknown settings");
        assert!(TuiSettings::load(&path).is_err());

        fs::write(&path, "version = 2\n").expect("new settings version");
        assert!(TuiSettings::load(&path).is_err());

        fs::write(&path, "version = 1\nauto_diagnose_on_startup = false\n")
            .expect("missing setting");
        assert!(TuiSettings::load(&path).is_err());
    }
}
