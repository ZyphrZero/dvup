use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::{ProcessAction, ProcessRule, Tool},
    error::{Error, Result},
    state::StateDirs,
};

static JOB_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// A command stored in a durable update job.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
}

/// A process currently preventing an update.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LockingProcess {
    pub pid: u32,
    pub name: String,
    pub start_time: u64,
}

/// Current state of a durable job.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    WaitingForLocks {
        processes: Vec<LockingProcess>,
    },
    TerminatingProcesses {
        processes: Vec<LockingProcess>,
    },
    Running {
        attempt: u32,
    },
    Succeeded {
        exit_code: i32,
    },
    Failed {
        message: String,
        exit_code: Option<i32>,
    },
}

impl JobStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WaitingForLocks { .. } => "waiting_for_locks",
            Self::TerminatingProcesses { .. } => "terminating_processes",
            Self::Running { .. } => "running",
            Self::Succeeded { .. } => "succeeded",
            Self::Failed { .. } => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded { .. } | Self::Failed { .. })
    }
}

/// Durable description and status of an update.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Job {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub command: CommandSpec,
    #[serde(default = "legacy_resource_group")]
    pub resource_group: String,
    pub process_rules: Vec<ProcessRule>,
    pub lock_timeout_secs: u64,
    pub retries: u32,
    pub retry_delay_secs: u64,
    pub status: JobStatus,
}

fn legacy_resource_group() -> String {
    "default".to_owned()
}

impl Job {
    /// Creates a durable job from a configured tool.
    pub fn from_tool(name: String, tool: Tool, working_directory: PathBuf) -> Self {
        let now = now_unix_ms();
        let process_rules = tool.process_rules();
        let resource_group = tool.resource_group.clone().unwrap_or_else(|| name.clone());
        Self {
            schema_version: 1,
            id: new_job_id(now),
            name,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            command: CommandSpec {
                program: tool.program,
                args: tool.args,
                working_directory,
            },
            resource_group,
            process_rules,
            lock_timeout_secs: tool.lock_timeout_secs,
            retries: tool.retries,
            retry_delay_secs: tool.retry_delay_secs,
            status: JobStatus::Pending,
        }
    }

    pub fn set_status(&mut self, status: JobStatus) {
        self.status = status;
        self.updated_at_unix_ms = now_unix_ms();
    }

    /// Converts configured wait rules to terminate rules for an explicit
    /// force-stop strategy, while refusing an unscoped Node termination.
    pub fn terminate_waiting_processes(&mut self) -> Result<usize> {
        if self.process_rules.iter().any(|rule| {
            rule.action == ProcessAction::Wait
                && rule.command_contains.is_none()
                && rule
                    .name
                    .trim()
                    .trim_end_matches(".exe")
                    .eq_ignore_ascii_case("node")
        }) {
            return Err(Error::Message(
                "terminate strategy cannot stop every Node process; add a command filter"
                    .to_owned(),
            ));
        }

        let mut changed = 0;
        for rule in &mut self.process_rules {
            if rule.action == ProcessAction::Wait {
                rule.action = ProcessAction::Terminate;
                changed += 1;
            }
        }
        Ok(changed)
    }
}

/// Persistence operations for jobs and their logs.
#[derive(Clone, Debug)]
pub struct JobStore {
    dirs: StateDirs,
}

impl JobStore {
    pub fn new(dirs: StateDirs) -> Result<Self> {
        dirs.ensure()?;
        Ok(Self { dirs })
    }

    pub fn dirs(&self) -> &StateDirs {
        &self.dirs
    }

    pub fn save(&self, job: &Job) -> Result<()> {
        let final_path = self.dirs.job_path(&job.id);
        let temporary_path = final_path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(job)?;

        {
            let mut file = fs::File::create(&temporary_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }

        replace_file(&temporary_path, &final_path)?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Job> {
        let path = self.dirs.job_path(id);
        if !path.is_file() {
            return Err(Error::JobNotFound(id.to_owned()));
        }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    pub fn list(&self) -> Result<Vec<Job>> {
        let mut jobs: Vec<Job> = Vec::new();
        for entry in fs::read_dir(self.dirs.jobs_dir())? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            jobs.push(serde_json::from_slice(&fs::read(path)?)?);
        }
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at_unix_ms));
        Ok(jobs)
    }

    pub fn append_log(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let mut options = fs::OpenOptions::new();
        let mut file = options
            .create(true)
            .append(true)
            .open(self.dirs.log_path(id))?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(())
    }

    pub fn read_log(&self, id: &str) -> Result<Vec<u8>> {
        let path = self.dirs.log_path(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        Ok(fs::read(path)?)
    }
}

fn new_job_id(now: u128) -> String {
    let sequence = JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{now}-{}-{sequence}", std::process::id())
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn test_tool() -> Tool {
        Tool {
            program: "npm".to_owned(),
            args: vec!["--version".to_owned()],
            lock_processes: vec!["node".to_owned()],
            processes: Vec::new(),
            lock_timeout_secs: 10,
            retries: 2,
            retry_delay_secs: 1,
            platforms: Vec::new(),
            resource_group: None,
        }
    }

    #[test]
    fn saves_updates_and_lists_jobs() {
        let temporary = TempDir::new().expect("temp dir");
        let store =
            JobStore::new(StateDirs::at(temporary.path().to_path_buf())).expect("create store");
        let mut job = Job::from_tool(
            "npm".to_owned(),
            test_tool(),
            temporary.path().to_path_buf(),
        );

        store.save(&job).expect("save pending job");
        job.set_status(JobStatus::Succeeded { exit_code: 0 });
        store.save(&job).expect("save completed job");

        let loaded = store.load(&job.id).expect("load job");
        assert!(loaded.status.is_terminal());
        assert_eq!(loaded.resource_group, "npm");
        assert_eq!(store.list().expect("list jobs").len(), 1);
    }

    #[test]
    fn terminate_strategy_converts_only_safe_wait_rules() {
        let mut tool = test_tool();
        tool.lock_processes = vec!["codex".to_owned()];
        tool.processes = vec![ProcessRule {
            name: "node".to_owned(),
            command_contains: Some("@openai/codex".to_owned()),
            action: crate::config::ProcessAction::Wait,
            terminate_grace_secs: 3,
        }];
        let mut job = Job::from_tool("codex".to_owned(), tool, PathBuf::from("."));

        assert_eq!(job.terminate_waiting_processes().expect("safe override"), 2);
        assert!(
            job.process_rules
                .iter()
                .all(|rule| rule.action == crate::config::ProcessAction::Terminate)
        );

        let mut unsafe_tool = test_tool();
        unsafe_tool.lock_processes = vec!["node".to_owned()];
        let mut unsafe_job = Job::from_tool("unsafe".to_owned(), unsafe_tool, PathBuf::from("."));
        assert!(unsafe_job.terminate_waiting_processes().is_err());
    }

    #[test]
    fn appends_log_chunks() {
        let temporary = TempDir::new().expect("temp dir");
        let store =
            JobStore::new(StateDirs::at(temporary.path().to_path_buf())).expect("create store");

        store.append_log("example", b"first\n").expect("first log");
        store
            .append_log("example", b"second\n")
            .expect("second log");

        assert_eq!(
            store.read_log("example").expect("read log"),
            b"first\nsecond\n"
        );
    }
}
