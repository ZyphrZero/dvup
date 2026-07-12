use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

use crate::error::{Error, Result};

/// Filesystem locations used to persist background jobs.
#[derive(Clone, Debug)]
pub struct StateDirs {
    root: PathBuf,
}

impl StateDirs {
    /// Resolves the per-user state location.
    pub fn discover() -> Result<Self> {
        let project =
            ProjectDirs::from("dev", "", "dvup").ok_or(Error::StateDirectoryUnavailable)?;
        Ok(Self {
            root: project.data_local_dir().to_path_buf(),
        })
    }

    /// Creates a state layout rooted at an explicit path supplied at runtime.
    pub fn at_runtime(root: PathBuf) -> Self {
        Self { root }
    }

    /// Creates a state layout rooted at an explicit path.
    #[cfg(test)]
    pub fn at(root: PathBuf) -> Self {
        Self::at_runtime(root)
    }

    /// Ensures all state directories exist.
    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(self.jobs_dir())?;
        fs::create_dir_all(self.workers_dir())?;
        fs::create_dir_all(self.locks_dir())?;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    pub fn workers_dir(&self) -> PathBuf {
        self.root.join("workers")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    pub fn job_path(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.json"))
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.log"))
    }

    pub fn custom_config_path(&self) -> PathBuf {
        self.root.join("dvup_custom.toml")
    }

    pub fn settings_path(&self) -> PathBuf {
        self.root.join("settings.toml")
    }

    pub fn release_state_path(&self) -> PathBuf {
        self.root.join("github-releases.json")
    }

    pub fn resource_lock_path(&self, resource_group: &str) -> PathBuf {
        let mut safe_name: String = resource_group
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || "-_.".contains(character) {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        if safe_name.is_empty() || safe_name == "." || safe_name == ".." {
            safe_name = "default".to_owned();
        }
        self.locks_dir().join(format!("{safe_name}.lock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_lock_path_stays_inside_lock_directory() {
        let state = StateDirs::at(PathBuf::from("state"));

        assert_eq!(
            state.resource_lock_path("../../outside"),
            PathBuf::from("state/locks/.._.._outside.lock")
        );
        assert_eq!(
            state.resource_lock_path("node-global"),
            PathBuf::from("state/locks/node-global.lock")
        );
    }

    #[test]
    fn discovered_state_path_never_repeats_the_application_name() {
        let state = StateDirs::discover().expect("discover state directory");
        let components = state
            .root()
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            !components.windows(2).any(|pair| {
                pair[0].eq_ignore_ascii_case("dvup") && pair[1].eq_ignore_ascii_case("dvup")
            }),
            "state directory repeats the application name: {}",
            state.root().display()
        );

        #[cfg(windows)]
        assert!(
            state.root().ends_with(Path::new("dvup").join("data")),
            "unexpected Windows state directory: {}",
            state.root().display()
        );
        #[cfg(target_os = "macos")]
        assert!(
            state.root().ends_with("dev.dvup"),
            "unexpected macOS state directory: {}",
            state.root().display()
        );
        #[cfg(target_os = "linux")]
        assert!(
            state.root().ends_with("dvup"),
            "unexpected Linux state directory: {}",
            state.root().display()
        );
    }
}
