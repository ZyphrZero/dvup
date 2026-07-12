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
            ProjectDirs::from("dev", "dvup", "dvup").ok_or(Error::StateDirectoryUnavailable)?;
        let root = project.data_local_dir().to_path_buf();
        // Keep existing custom tools and in-flight jobs usable after the rename.
        let legacy = ProjectDirs::from("dev", "kvdev", "kvdev")
            .ok_or(Error::StateDirectoryUnavailable)?
            .data_local_dir()
            .to_path_buf();
        Ok(Self {
            root: preferred_root(root, legacy),
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
        self.root.join("custom.toml")
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

fn preferred_root(current: PathBuf, legacy: PathBuf) -> PathBuf {
    if current.exists() || !legacy.exists() {
        current
    } else {
        legacy
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
    fn state_root_prefers_dvup_and_falls_back_to_kvdev() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let current = temporary.path().join("dvup");
        let legacy = temporary.path().join("kvdev");

        fs::create_dir_all(&legacy).expect("create legacy state");
        assert_eq!(preferred_root(current.clone(), legacy.clone()), legacy);

        fs::create_dir_all(&current).expect("create current state");
        assert_eq!(preferred_root(current.clone(), legacy), current);
    }
}
