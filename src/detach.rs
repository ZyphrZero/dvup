use std::fs;

#[cfg(not(windows))]
use std::process::{Command, Stdio};

use crate::{
    error::{Error, Result},
    job::Job,
    state::StateDirs,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerLaunch {
    Spawned,
    AlreadyRunning,
}

/// Copies the current executable out of the way and starts a detached worker.
pub fn spawn_worker(job: &Job, dirs: &StateDirs) -> Result<()> {
    dirs.ensure()?;
    let source = std::env::current_exe()?;
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    let worker = dirs
        .workers_dir()
        .join(format!("dvup-worker-{}{extension}", job.id));
    fs::copy(source, &worker)?;
    let worker = fs::canonicalize(worker)?;
    let state_root = fs::canonicalize(dirs.root())?;
    spawn_detached_worker(&worker, &state_root, &job.id)
}

/// Starts a recovery worker unless the per-job worker executable is still
/// locked by an existing worker process.
pub fn ensure_worker(job: &Job, dirs: &StateDirs) -> Result<WorkerLaunch> {
    match spawn_worker(job, dirs) {
        Ok(()) => Ok(WorkerLaunch::Spawned),
        Err(Error::Io(error)) if worker_is_still_running(&error) => {
            Ok(WorkerLaunch::AlreadyRunning)
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(windows))]
fn spawn_detached_worker(
    worker: &std::path::Path,
    state_root: &std::path::Path,
    job_id: &str,
) -> Result<()> {
    let mut command = Command::new(worker);
    command
        .arg("--state-dir")
        .arg(state_root)
        .arg("__worker")
        .arg(job_id)
        .current_dir(state_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_detached(&mut command);
    command.spawn()?;
    Ok(())
}

#[cfg(windows)]
fn spawn_detached_worker(
    worker: &std::path::Path,
    state_root: &std::path::Path,
    job_id: &str,
) -> Result<()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
            STARTUPINFOW,
        },
    };

    let arguments = [
        worker.as_os_str(),
        OsStr::new("--state-dir"),
        state_root.as_os_str(),
        OsStr::new("__worker"),
        OsStr::new(job_id),
    ];
    let mut command_line = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            command_line.push(b' ' as u16);
        }
        command_line.extend(quote_windows_argument(argument));
    }
    command_line.push(0);

    let mut application: Vec<u16> = worker.as_os_str().encode_wide().collect();
    application.push(0);
    let mut current_directory: Vec<u16> = state_root.as_os_str().encode_wide().collect();
    current_directory.push(0);

    // SAFETY: both structures are plain Windows API data structures whose
    // documented initialization is zeroed memory plus STARTUPINFO.cb.
    let mut startup_info: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // SAFETY: PROCESS_INFORMATION is an output-only plain data structure.
    let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: all UTF-16 strings are NUL-terminated and live through the call;
    // command_line is writable as required by CreateProcessW. Passing FALSE
    // for handle inheritance prevents the worker from keeping caller pipes or
    // terminal handles alive after the scheduling process exits.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS,
            ptr::null(),
            current_directory.as_ptr(),
            &startup_info,
            &mut process_info,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    // SAFETY: CreateProcessW returned owned process and thread handles. Closing
    // them does not terminate the detached worker.
    unsafe {
        CloseHandle(process_info.hThread);
        CloseHandle(process_info.hProcess);
    }
    Ok(())
}

#[cfg(windows)]
fn quote_windows_argument(argument: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let value: Vec<u16> = argument.encode_wide().collect();
    let needs_quotes = value.is_empty()
        || value.iter().any(|character| {
            *character == b' ' as u16 || *character == b'\t' as u16 || *character == b'"' as u16
        });
    if !needs_quotes {
        return value;
    }

    let mut quoted = vec![b'"' as u16];
    let mut backslashes = 0_usize;
    for character in value {
        if character == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if character == b'"' as u16 {
            quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            quoted.push(character);
            backslashes = 0;
            continue;
        }
        quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        backslashes = 0;
        quoted.push(character);
    }
    quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    quoted.push(b'"' as u16);
    quoted
}

/// Removes worker copies left by completed runs. A currently running Windows
/// worker remains locked and is naturally skipped until the next invocation.
pub fn cleanup_workers(dirs: &StateDirs) -> Result<()> {
    dirs.ensure()?;
    for entry in fs::read_dir(dirs.workers_dir())? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if worker_is_still_running(&error) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn worker_is_still_running(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ResourceBusy
    ) {
        return true;
    }
    #[cfg(windows)]
    {
        // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION.
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(unix)]
fn configure_detached(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: setsid is async-signal-safe and only creates a new session in
    // the child process immediately before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(any(unix, windows)))]
fn configure_detached(_command: &mut Command) {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn quotes_windows_worker_arguments() {
        let cases = [
            ("plain", "plain"),
            ("two words", "\"two words\""),
            (r#"a\"b"#, r#""a\\\"b""#),
            ("trailing slash\\", r#""trailing slash\\""#),
        ];

        for (input, expected) in cases {
            let quoted = String::from_utf16(&quote_windows_argument(OsStr::new(input)))
                .expect("valid UTF-16");
            assert_eq!(quoted, expected, "unexpected quoting for {input:?}");
        }
    }
}
