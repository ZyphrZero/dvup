use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
};

use crate::{
    config::Tool,
    datetime,
    error::{Error, Result},
    job::CommandSpec,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolReadiness {
    Installed,
    TargetMissing,
    UpdaterMissing,
    Unsupported,
}

pub fn probe_spec(tool: &Tool, working_directory: &std::path::Path) -> CommandSpec {
    CommandSpec {
        program: tool.probe.program.clone(),
        args: tool.probe.args.clone(),
        working_directory: working_directory.to_path_buf(),
    }
}

pub fn update_spec(tool: &Tool, working_directory: &std::path::Path) -> CommandSpec {
    CommandSpec {
        program: tool.program.clone(),
        args: tool.args.clone(),
        working_directory: working_directory.to_path_buf(),
    }
}

pub fn tool_readiness(tool: &Tool, working_directory: &std::path::Path) -> ToolReadiness {
    if !tool.supports_current_platform() {
        ToolReadiness::Unsupported
    } else if !is_available(&probe_spec(tool, working_directory)) {
        ToolReadiness::TargetMissing
    } else if !is_available(&update_spec(tool, working_directory)) {
        ToolReadiness::UpdaterMissing
    } else {
        ToolReadiness::Installed
    }
}

/// Captured result of an update command.
#[derive(Debug)]
pub struct CommandResult {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandResult {
    pub fn exit_code(&self) -> Option<i32> {
        self.status.code()
    }

    pub fn is_lock_failure(&self) -> bool {
        if self.status.success() {
            return false;
        }
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr)
        )
        .to_lowercase();
        contains_lock_failure(&combined)
    }

    pub fn is_permission_failure(&self) -> bool {
        if self.status.success() {
            return false;
        }
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr)
        )
        .to_lowercase();
        contains_permission_failure(&combined)
    }
}

const LOCK_FAILURE_MARKERS: &[&str] = &[
    "ebusy",
    "resource busy",
    "being used by another process",
    "process cannot access the file",
    "file is locked",
    "text file busy",
    "sharing violation",
];

const PERMISSION_FAILURE_MARKERS: &[&str] = &[
    "eacces",
    "permission denied",
    "operation not permitted",
    "access is denied",
    "requires elevated privileges",
];

fn contains_lock_failure(output: &str) -> bool {
    LOCK_FAILURE_MARKERS
        .iter()
        .any(|marker| output.contains(marker))
        || (output.contains("eperm")
            && ["unlink", "rename", "rmdir"]
                .iter()
                .any(|operation| output.contains(operation)))
}

fn contains_permission_failure(output: &str) -> bool {
    PERMISSION_FAILURE_MARKERS
        .iter()
        .any(|marker| output.contains(marker))
}

/// Executes a command without invoking a shell and captures both output streams.
pub fn run(spec: &CommandSpec) -> Result<CommandResult> {
    capture_command(spec, prepare_command(spec))
}

/// Executes an already resolved native executable without an intermediary shell.
pub(crate) fn run_direct(spec: &CommandSpec) -> Result<CommandResult> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    configure_no_window(&mut command);
    capture_command(spec, command)
}

fn capture_command(spec: &CommandSpec, mut command: Command) -> Result<CommandResult> {
    let output = command
        .current_dir(&spec.working_directory)
        .output()
        .map_err(|source| Error::CommandStart {
            program: spec.program.clone(),
            source,
        })?;

    Ok(CommandResult {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Returns whether the command can be resolved without executing it.
pub fn is_available(spec: &CommandSpec) -> bool {
    let resolved = resolve_program(spec);
    #[cfg(windows)]
    {
        resolved.is_file() || powershell_resolves_command(spec)
    }
    #[cfg(unix)]
    {
        is_unix_executable(&resolved, spec)
    }
    #[cfg(not(any(unix, windows)))]
    {
        resolved.is_file()
    }
}

#[cfg(not(windows))]
fn prepare_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(resolve_program(spec));
    command.args(&spec.args);
    command
}

#[cfg(windows)]
fn prepare_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        .arg(powershell_invocation(spec));
    configure_no_window(&mut command);
    command
}

/// Prevents Windows from creating a console window for captured/background
/// child processes while preserving stdout/stderr pipes.
#[cfg(windows)]
pub fn configure_no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn configure_no_window(_command: &mut Command) {}

#[cfg(windows)]
fn powershell_invocation(spec: &CommandSpec) -> String {
    let arguments = std::iter::once(spec.program.as_str())
        .chain(spec.args.iter().map(String::as_str))
        .map(powershell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    format!("$ProgressPreference = 'SilentlyContinue'; & {arguments}")
}

#[cfg(windows)]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(windows)]
fn powershell_resolves_command(spec: &CommandSpec) -> bool {
    let probe = format!(
        "Get-Command -Name {} -ErrorAction Stop | Out-Null",
        powershell_quote(&spec.program)
    );
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        .arg(probe)
        .current_dir(&spec.working_directory)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_no_window(&mut command);
    command.status().is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn is_unix_executable(program: &std::path::Path, spec: &CommandSpec) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let has_directory = program.is_absolute() || program.components().count() > 1;
    let candidates: Vec<_> = if has_directory {
        vec![if program.is_absolute() {
            program.to_path_buf()
        } else {
            spec.working_directory.join(program)
        }]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join(program))
                    .collect()
            })
            .unwrap_or_default()
    };
    candidates.into_iter().any(|candidate| {
        candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(not(windows))]
fn resolve_program(spec: &CommandSpec) -> PathBuf {
    PathBuf::from(&spec.program)
}

#[cfg(windows)]
fn resolve_program(spec: &CommandSpec) -> PathBuf {
    let program = std::path::Path::new(&spec.program);
    let has_directory = program.is_absolute() || program.components().count() > 1;
    let mut bases = Vec::new();

    if has_directory {
        bases.push(if program.is_absolute() {
            program.to_path_buf()
        } else {
            spec.working_directory.join(program)
        });
    } else {
        bases.push(spec.working_directory.join(program));
        if let Some(path) = std::env::var_os("PATH") {
            bases.extend(std::env::split_paths(&path).map(|directory| directory.join(program)));
        }
    }

    for base in bases {
        if base.extension().is_some() {
            if base.is_file() {
                return base;
            }
            continue;
        }
        for extension in windows_executable_extensions() {
            let candidate = base.with_extension(extension);
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    program.to_path_buf()
}

#[cfg(windows)]
fn windows_executable_extensions() -> Vec<String> {
    const SUPPORTED: &[&str] = &["com", "exe", "bat", "cmd"];
    let Ok(configured) = std::env::var("PATHEXT") else {
        return Vec::new();
    };
    configured
        .split(';')
        .map(|extension| extension.trim().trim_start_matches('.').to_lowercase())
        .filter(|extension| SUPPORTED.contains(&extension.as_str()))
        .collect()
}

/// Appends a command result to a job log.
pub fn append_to_log(path: &std::path::Path, result: &CommandResult) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&datetime::timestamp_bytes(&result.stdout))?;
    file.write_all(&datetime::timestamp_bytes(&result.stderr))?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn resolves_windows_pathext_from_working_directory() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let command_file = temporary.path().join("example.CMD");
        std::fs::write(&command_file, "@echo off\r\n").expect("write command");
        let spec = crate::job::CommandSpec {
            program: "example".to_owned(),
            args: Vec::new(),
            working_directory: temporary.path().to_path_buf(),
        };

        let resolved = super::resolve_program(&spec);
        assert_eq!(
            std::fs::canonicalize(resolved).expect("canonical resolved command"),
            std::fs::canonicalize(command_file).expect("canonical expected command")
        );
    }

    #[test]
    fn classifies_lock_failures() {
        let cases = [
            ("npm ERR! code EBUSY", true),
            ("EPERM: operation not permitted, unlink 'codex.exe'", true),
            ("permission denied while opening config", false),
            ("network timeout", false),
        ];

        for (output, expected) in cases {
            assert_eq!(
                super::contains_lock_failure(&output.to_lowercase()),
                expected,
                "unexpected classification for {output:?}"
            );
        }
    }

    #[test]
    fn classifies_permission_failures() {
        let cases = [
            ("EACCES: permission denied", true),
            ("requires elevated privileges", true),
            ("package not found", false),
        ];

        for (output, expected) in cases {
            assert_eq!(
                super::contains_permission_failure(&output.to_lowercase()),
                expected,
                "unexpected classification for {output:?}"
            );
        }
    }

    #[test]
    fn current_executable_is_available() {
        let executable = std::env::current_exe().expect("current executable");
        let spec = crate::job::CommandSpec {
            program: executable.to_string_lossy().into_owned(),
            args: Vec::new(),
            working_directory: std::env::current_dir().expect("current directory"),
        };

        assert!(super::is_available(&spec));
    }

    #[test]
    fn readiness_distinguishes_the_target_from_its_updater() {
        let current = std::env::current_exe()
            .expect("current executable")
            .to_string_lossy()
            .into_owned();
        let working_directory = std::env::current_dir().expect("current directory");
        let mut tool = crate::config::Tool::custom("example", current.clone(), Vec::new());
        tool.probe.program = current;

        assert_eq!(
            super::tool_readiness(&tool, &working_directory),
            super::ToolReadiness::Installed
        );

        tool.program = "dvup-missing-updater-96cf27db".to_owned();
        assert_eq!(
            super::tool_readiness(&tool, &working_directory),
            super::ToolReadiness::UpdaterMissing
        );

        tool.probe.program = "dvup-missing-target-96cf27db".to_owned();
        assert_eq!(
            super::tool_readiness(&tool, &working_directory),
            super::ToolReadiness::TargetMissing
        );
    }

    #[test]
    fn missing_executable_is_unavailable() {
        let spec = crate::job::CommandSpec {
            program: "dvup-definitely-missing-command-8fcb4ce4".to_owned(),
            args: Vec::new(),
            working_directory: std::env::current_dir().expect("current directory"),
        };

        assert!(!super::is_available(&spec));
    }

    #[cfg(windows)]
    #[test]
    fn builds_safe_powershell_invocation() {
        let spec = crate::job::CommandSpec {
            program: "claude".to_owned(),
            args: vec!["install".to_owned(), "it's-safe".to_owned()],
            working_directory: std::env::current_dir().expect("current directory"),
        };

        assert_eq!(
            super::powershell_invocation(&spec),
            "$ProgressPreference = 'SilentlyContinue'; & 'claude' 'install' 'it''s-safe'"
        );
    }

    #[cfg(windows)]
    #[test]
    fn executes_powershell_cmdlet() {
        let spec = crate::job::CommandSpec {
            program: "Write-Output".to_owned(),
            args: vec!["dvup-powershell-ok".to_owned()],
            working_directory: std::env::current_dir().expect("current directory"),
        };

        assert!(super::is_available(&spec));
        let result = super::run(&spec).expect("run PowerShell cmdlet");
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("dvup-powershell-ok"));
    }
}
