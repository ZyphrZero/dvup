use std::{
    fs, thread,
    time::{Duration, Instant},
};

use fs2::FileExt;

use crate::{
    command,
    config::ProcessAction,
    datetime,
    error::{Error, Result},
    job::{Job, JobStatus, JobStore},
    process,
};

/// Runs one previously persisted background job.
pub fn run(job_id: &str, store: &JobStore) -> Result<()> {
    match run_inner(job_id, store) {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(state_error) = record_unexpected_worker_failure(job_id, store, &error) {
                return Err(Error::Message(format!(
                    "{error}; also failed to persist the worker failure: {state_error}"
                )));
            }
            Err(error)
        }
    }
}

fn run_inner(job_id: &str, store: &JobStore) -> Result<()> {
    let mut job = store.load(job_id)?;
    if job.status.is_terminal() {
        return Ok(());
    }

    log_line(store, job_id, &format!("job {} started", job.id))?;
    handle_process_rules(&mut job, store)?;

    let lock_path = store.dirs().resource_lock_path(&job.resource_group);
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock_file.lock_exclusive()?;

    // Another recovery worker may have completed the job while this worker
    // was waiting for the resource lock. Reload under the lock so a recovered
    // job is still executed at most once.
    job = store.load(job_id)?;
    if job.status.is_terminal() {
        FileExt::unlock(&lock_file)?;
        return Ok(());
    }

    let result = execute_with_retries(&mut job, store);
    FileExt::unlock(&lock_file)?;
    result
}

fn record_unexpected_worker_failure(job_id: &str, store: &JobStore, error: &Error) -> Result<()> {
    let mut job = store.load(job_id)?;
    if job.status.is_terminal() {
        return Ok(());
    }
    fail_job(
        &mut job,
        store,
        format!("background worker stopped unexpectedly: {error}"),
        None,
    )
}

/// Applies only terminate rules for a queued job without executing its update.
/// The transition is persisted before signaling processes so an interrupted
/// recovery cannot leave the durable state claiming it is still waiting.
pub fn apply_terminate_rules(job: &mut Job, store: &JobStore) -> Result<usize> {
    let matches = process::find_matching_processes(&job.process_rules)
        .into_iter()
        .filter(|matched| matched.action == ProcessAction::Terminate)
        .collect::<Vec<_>>();

    if matches.is_empty() {
        return Ok(0);
    }

    let processes = matches
        .iter()
        .map(|matched| matched.process.clone())
        .collect::<Vec<_>>();
    log_line(
        store,
        &job.id,
        &format!(
            "terminating matching processes: {}",
            describe_processes(&processes)
        ),
    )?;
    job.set_status(JobStatus::TerminatingProcesses { processes });
    store.save(job)?;

    for matched in &matches {
        if let Err(error) = terminate_process(matched, store, &job.id) {
            let message = error.to_string();
            fail_job(job, store, message.clone(), None)?;
            return Err(Error::Message(message));
        }
    }
    Ok(matches.len())
}

fn handle_process_rules(job: &mut Job, store: &JobStore) -> Result<()> {
    let started = Instant::now();
    let timeout = Duration::from_secs(job.lock_timeout_secs);
    let mut last_pids = Vec::new();

    loop {
        // A TUI policy switch can update the persisted rules while this
        // detached worker is waiting. Reload only the rules and retain this
        // worker's in-memory status progression.
        job.process_rules = store.load(&job.id)?.process_rules;
        let matches = process::find_matching_processes(&job.process_rules);
        if matches.is_empty() {
            return Ok(());
        }

        let failed: Vec<_> = matches
            .iter()
            .filter(|matched| matched.action == ProcessAction::Fail)
            .map(|matched| matched.process.clone())
            .collect();
        if !failed.is_empty() {
            let message = format!(
                "process policy rejected the update because {} is running",
                describe_processes(&failed)
            );
            fail_job(job, store, message.clone(), None)?;
            return Err(Error::Message(message));
        }

        let terminating: Vec<_> = matches
            .iter()
            .filter(|matched| matched.action == ProcessAction::Terminate)
            .cloned()
            .collect();
        if !terminating.is_empty() {
            let processes = terminating
                .iter()
                .map(|matched| matched.process.clone())
                .collect::<Vec<_>>();
            log_line(
                store,
                &job.id,
                &format!(
                    "terminating matching processes: {}",
                    describe_processes(&processes)
                ),
            )?;
            job.set_status(JobStatus::TerminatingProcesses {
                processes: processes.clone(),
            });
            store.save(job)?;
            for matched in &terminating {
                if let Err(error) = terminate_process(matched, store, &job.id) {
                    let message = error.to_string();
                    fail_job(job, store, message.clone(), None)?;
                    return Err(Error::Message(message));
                }
            }
            ensure_process_timeout(job, store, started, timeout)?;
            continue;
        }

        let processes: Vec<_> = matches
            .iter()
            .filter(|matched| matched.action == ProcessAction::Wait)
            .map(|matched| matched.process.clone())
            .collect();

        let pids: Vec<_> = processes.iter().map(|process| process.pid).collect();
        if pids != last_pids {
            log_line(
                store,
                &job.id,
                &format!(
                    "waiting for matching processes: {}",
                    describe_processes(&processes)
                ),
            )?;
            job.set_status(JobStatus::WaitingForLocks {
                processes: processes.clone(),
            });
            store.save(job)?;
            last_pids = pids;
        }

        ensure_process_timeout(job, store, started, timeout)?;
        thread::sleep(Duration::from_secs(1));
    }
}

fn terminate_process(
    matched: &process::MatchedProcess,
    store: &JobStore,
    job_id: &str,
) -> Result<()> {
    let target = &matched.process;
    let graceful_requested = process::request_termination(target);
    if graceful_requested {
        let deadline = Instant::now() + Duration::from_secs(matched.terminate_grace_secs);
        while process::is_alive(target) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
    }
    if process::is_alive(target) {
        log_line(
            store,
            job_id,
            &format!(
                "process {} ({}) did not exit gracefully; forcing termination",
                target.name, target.pid
            ),
        )?;
        if !process::force_kill(target) && process::is_alive(target) {
            return Err(Error::Message(format!(
                "failed to terminate {} ({})",
                target.name, target.pid
            )));
        }
    }
    Ok(())
}

fn ensure_process_timeout(
    job: &mut Job,
    store: &JobStore,
    started: Instant,
    timeout: Duration,
) -> Result<()> {
    if started.elapsed() < timeout {
        return Ok(());
    }
    let message = format!(
        "timed out after {} seconds applying process policies",
        job.lock_timeout_secs
    );
    fail_job(job, store, message.clone(), None)?;
    Err(Error::Message(message))
}

fn describe_processes(processes: &[crate::job::LockingProcess]) -> String {
    processes
        .iter()
        .map(|process| format!("{} ({})", process.name, process.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

fn execute_with_retries(job: &mut Job, store: &JobStore) -> Result<()> {
    let attempts = job.retries.saturating_add(1);
    for attempt in 1..=attempts {
        job.set_status(JobStatus::Running { attempt });
        store.save(job)?;
        log_line(
            store,
            &job.id,
            &format!(
                "attempt {attempt}/{attempts}: {} {}",
                job.command.program,
                job.command.args.join(" ")
            ),
        )?;

        let result = match command::run_with_network(&job.command, &job.network) {
            Ok(result) => result,
            Err(error) => {
                let message = error.to_string();
                fail_job(job, store, message.clone(), None)?;
                return Err(Error::Message(message));
            }
        };
        command::append_to_log(&store.dirs().log_path(&job.id), &result)?;

        if result.status.success() {
            let exit_code = result.exit_code().unwrap_or(0);
            job.set_status(JobStatus::Succeeded { exit_code });
            store.save(job)?;
            log_line(store, &job.id, "job succeeded")?;
            return Ok(());
        }

        if result.is_permission_failure() && !result.is_lock_failure() {
            let message = "update failed because the package manager lacks permission; configure a user-owned global prefix or run dvup from an elevated terminal".to_owned();
            fail_job(job, store, message.clone(), result.exit_code())?;
            return Err(Error::Message(message));
        }

        if attempt == attempts {
            let message = format!("command failed after {attempts} attempt(s)");
            fail_job(job, store, message.clone(), result.exit_code())?;
            return Err(Error::Message(message));
        }

        let delay = retry_delay(job.retry_delay_secs, attempt);
        log_line(
            store,
            &job.id,
            &format!("command failed; retrying in {} second(s)", delay.as_secs()),
        )?;
        thread::sleep(delay);

        if result.is_lock_failure() {
            handle_process_rules(job, store)?;
        }
    }

    Err(Error::Message("job ended without a result".to_owned()))
}

fn retry_delay(base_secs: u64, completed_attempt: u32) -> Duration {
    let exponent = completed_attempt.saturating_sub(1).min(4);
    let multiplier = 1_u64 << exponent;
    Duration::from_secs(base_secs.saturating_mul(multiplier).min(30))
}

fn fail_job(
    job: &mut Job,
    store: &JobStore,
    message: String,
    exit_code: Option<i32>,
) -> Result<()> {
    let log_message = format!("job failed: {message}");
    job.set_status(JobStatus::Failed { message, exit_code });
    store.save(job)?;
    log_line(store, &job.id, &log_message)
}

fn log_line(store: &JobStore, id: &str, line: &str) -> Result<()> {
    store.append_log(id, datetime::timestamp_line(line).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_is_capped() {
        let cases = [(1, 2), (2, 4), (3, 8), (4, 16), (5, 30), (20, 30)];
        for (attempt, expected_secs) in cases {
            assert_eq!(retry_delay(2, attempt), Duration::from_secs(expected_secs));
        }
    }

    #[test]
    fn worker_log_lines_include_a_datetime() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let store = JobStore::new(crate::state::StateDirs::at(temporary.path().to_path_buf()))
            .expect("job store");

        log_line(&store, "timestamp-test", "worker event").expect("write log");

        let log = String::from_utf8(store.read_log("timestamp-test").expect("read log"))
            .expect("utf-8 log");
        assert_eq!(
            crate::datetime::strip_timestamp_prefix(&log),
            "worker event\n"
        );
    }

    #[test]
    fn infrastructure_errors_do_not_leave_jobs_non_terminal() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = crate::state::StateDirs::at(temporary.path().to_path_buf());
        let store = JobStore::new(state.clone()).expect("job store");
        let job = Job::from_tool(
            "broken-worker".to_owned(),
            crate::config::Tool::custom(
                "broken-worker",
                "definitely-not-used".to_owned(),
                Vec::new(),
            ),
            temporary.path().to_path_buf(),
            crate::settings::NetworkSettings::default(),
        );
        let job_id = job.id.clone();
        store.save(&job).expect("save job");

        fs::remove_dir_all(state.locks_dir()).expect("remove lock directory");
        fs::write(state.locks_dir(), b"not a directory").expect("block lock directory");

        assert!(run(&job_id, &store).is_err());
        let updated = store.load(&job_id).expect("load failed job");
        assert!(matches!(updated.status, JobStatus::Failed { .. }));
    }

    #[test]
    fn recovery_worker_skips_job_completed_while_waiting_for_resource_lock() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = crate::state::StateDirs::at(temporary.path().to_path_buf());
        let store = JobStore::new(state.clone()).expect("job store");
        let job = Job::from_tool(
            "recovery-lock".to_owned(),
            crate::config::Tool::custom(
                "recovery-lock",
                "this-command-must-not-run".to_owned(),
                Vec::new(),
            ),
            temporary.path().to_path_buf(),
            crate::settings::NetworkSettings::default(),
        );
        let job_id = job.id.clone();
        store.save(&job).expect("save job");

        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.resource_lock_path(&job.resource_group))
            .expect("open resource lock");
        lock_file.lock_exclusive().expect("hold resource lock");

        let worker_store = store.clone();
        let worker_job_id = job_id.clone();
        let worker = thread::spawn(move || run(&worker_job_id, &worker_store));
        let deadline = Instant::now() + Duration::from_secs(2);
        while store.read_log(&job_id).expect("read worker log").is_empty()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!store.read_log(&job_id).expect("read worker log").is_empty());

        let mut completed = store.load(&job_id).expect("load waiting job");
        completed.set_status(JobStatus::Succeeded { exit_code: 0 });
        store.save(&completed).expect("complete job externally");
        FileExt::unlock(&lock_file).expect("release resource lock");

        worker
            .join()
            .expect("worker thread")
            .expect("worker should skip completed job");
        assert!(matches!(
            store.load(&job_id).expect("load completed job").status,
            JobStatus::Succeeded { exit_code: 0 }
        ));
    }
}
