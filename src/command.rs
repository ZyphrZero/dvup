use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, ExitStatus},
};

#[cfg(windows)]
use std::collections::HashMap;

use crate::{
    config::Tool,
    datetime,
    error::{Error, Result},
    job::CommandSpec,
    settings::{NetworkSettings, ProxyMode},
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
    tool_readiness_many([tool], working_directory)[0]
}

pub fn tool_readiness_many<'a>(
    tools: impl IntoIterator<Item = &'a Tool>,
    working_directory: &std::path::Path,
) -> Vec<ToolReadiness> {
    let tools = tools.into_iter().collect::<Vec<_>>();
    let mut specs = Vec::with_capacity(tools.len().saturating_mul(2));
    let mut spec_indices = Vec::with_capacity(tools.len());
    for tool in &tools {
        if tool.supports_current_platform() {
            let probe_index = specs.len();
            specs.push(probe_spec(tool, working_directory));
            let update_index = specs.len();
            specs.push(update_spec(tool, working_directory));
            spec_indices.push(Some((probe_index, update_index)));
        } else {
            spec_indices.push(None);
        }
    }
    let available = commands_available(&specs);
    spec_indices
        .into_iter()
        .map(|indices| match indices {
            None => ToolReadiness::Unsupported,
            Some((probe, _)) if !available[probe] => ToolReadiness::TargetMissing,
            Some((_, update)) if !available[update] => ToolReadiness::UpdaterMissing,
            Some(_) => ToolReadiness::Installed,
        })
        .collect()
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

/// Executes a command with the application's exact network policy.
pub(crate) fn run_with_network(
    spec: &CommandSpec,
    network: &NetworkSettings,
) -> Result<CommandResult> {
    network.validate()?;
    let mut command = prepare_command(spec);
    apply_network_environment(&mut command, network);
    capture_command(spec, command)
}

const PROXY_ENVIRONMENT_VARIABLES: &[&str] = &[
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
];

fn apply_network_environment(command: &mut Command, network: &NetworkSettings) {
    if network.proxy_mode == ProxyMode::Environment {
        return;
    }
    for variable in PROXY_ENVIRONMENT_VARIABLES {
        command.env_remove(variable);
    }
    if network.proxy_mode == ProxyMode::Explicit {
        let proxy_url = network
            .proxy_url
            .as_deref()
            .expect("validated explicit proxy");
        for variable in [
            "ALL_PROXY",
            "all_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ] {
            command.env(variable, proxy_url);
        }
        if let Some(no_proxy) = network.no_proxy_value() {
            command.env("NO_PROXY", &no_proxy).env("no_proxy", no_proxy);
        }
    }
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
#[cfg(test)]
pub fn is_available(spec: &CommandSpec) -> bool {
    commands_available(std::slice::from_ref(spec))[0]
}

fn commands_available(specs: &[CommandSpec]) -> Vec<bool> {
    #[cfg(windows)]
    {
        let mut cache = HashMap::new();
        specs
            .iter()
            .map(|spec| {
                let key = (spec.program.to_lowercase(), spec.working_directory.clone());
                *cache
                    .entry(key)
                    .or_insert_with(|| resolve_program(spec).is_file())
            })
            .collect()
    }
    #[cfg(unix)]
    {
        specs
            .iter()
            .map(|spec| is_unix_executable(&resolve_program(spec), spec))
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        specs
            .iter()
            .map(|spec| resolve_program(spec).is_file())
            .collect()
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
    let program = resolve_program(spec);
    let is_batch = program
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_lowercase().as_str(), "bat" | "cmd"));
    let mut command = if is_batch {
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/S", "/C"]).arg(program);
        command
    } else {
        Command::new(program)
    };
    command.args(&spec.args);
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
    use std::ffi::{OsStr, OsString};

    use crate::settings::{NetworkSettings, ProxyMode};

    fn command_environment(
        command: &std::process::Command,
        name: &str,
    ) -> Option<Option<OsString>> {
        command
            .get_envs()
            .find(|(key, _)| {
                #[cfg(windows)]
                {
                    key.to_string_lossy().eq_ignore_ascii_case(name)
                }
                #[cfg(not(windows))]
                {
                    *key == OsStr::new(name)
                }
            })
            .map(|(_, value)| value.map(OsStr::to_os_string))
    }

    #[test]
    fn explicit_network_replaces_all_proxy_environment_variables() {
        let network = NetworkSettings {
            proxy_mode: ProxyMode::Explicit,
            proxy_url: Some("http://127.0.0.1:7890".to_owned()),
            no_proxy: vec!["localhost".to_owned(), ".example.com".to_owned()],
        };
        let mut command = std::process::Command::new("example");
        super::apply_network_environment(&mut command, &network);

        for name in [
            "ALL_PROXY",
            "all_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ] {
            assert_eq!(
                command_environment(&command, name),
                Some(Some(OsString::from("http://127.0.0.1:7890")))
            );
        }
        assert_eq!(
            command_environment(&command, "NO_PROXY"),
            Some(Some(OsString::from("localhost,.example.com")))
        );
    }

    #[test]
    fn direct_network_removes_all_proxy_environment_variables() {
        let network = NetworkSettings {
            proxy_mode: ProxyMode::Direct,
            proxy_url: None,
            no_proxy: Vec::new(),
        };
        let mut command = std::process::Command::new("example");
        super::apply_network_environment(&mut command, &network);

        for name in super::PROXY_ENVIRONMENT_VARIABLES {
            assert_eq!(command_environment(&command, name), Some(None));
        }
    }

    #[test]
    fn environment_network_does_not_override_command_environment() {
        let mut command = std::process::Command::new("example");
        command.env("HTTP_PROXY", "http://inherited.example:8080");
        super::apply_network_environment(&mut command, &NetworkSettings::default());

        assert_eq!(
            command_environment(&command, "HTTP_PROXY"),
            Some(Some(OsString::from("http://inherited.example:8080")))
        );
        assert_eq!(command_environment(&command, "HTTPS_PROXY"), None);
    }

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
    fn batched_readiness_preserves_tool_order_and_status() {
        let current = std::env::current_exe()
            .expect("current executable")
            .to_string_lossy()
            .into_owned();
        let working_directory = std::env::current_dir().expect("current directory");

        let mut installed = crate::config::Tool::custom("installed", current.clone(), Vec::new());
        installed.probe.program = current.clone();
        let mut updater_missing = installed.clone();
        updater_missing.program = "dvup-batch-missing-updater-96cf27db".to_owned();
        let mut target_missing = installed.clone();
        target_missing.probe.program = "dvup-batch-missing-target-96cf27db".to_owned();
        let tools = [&installed, &updater_missing, &target_missing];

        assert_eq!(
            super::tool_readiness_many(tools, &working_directory),
            [
                super::ToolReadiness::Installed,
                super::ToolReadiness::UpdaterMissing,
                super::ToolReadiness::TargetMissing,
            ]
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
    fn windows_batch_programs_execute_through_cmd() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let script = temporary.path().join("echo-argument.cmd");
        std::fs::write(&script, "@echo off\r\necho %~1\r\n").expect("write batch script");
        let spec = crate::job::CommandSpec {
            program: script.to_string_lossy().into_owned(),
            args: vec!["two words".to_owned()],
            working_directory: temporary.path().to_path_buf(),
        };

        let command = super::prepare_command(&spec);
        assert!(
            command
                .get_program()
                .to_string_lossy()
                .eq_ignore_ascii_case("cmd.exe")
        );
        let result =
            super::run_with_network(&spec, &NetworkSettings::default()).expect("run batch script");
        assert!(result.status.success());
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "two words");
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_programs_execute_without_powershell() {
        let executable = std::env::current_exe().expect("current executable");
        let spec = crate::job::CommandSpec {
            program: executable.to_string_lossy().into_owned(),
            args: Vec::new(),
            working_directory: std::env::current_dir().expect("current directory"),
        };

        let command = super::prepare_command(&spec);

        assert_eq!(command.get_program(), executable.as_os_str());
    }

    #[cfg(windows)]
    #[test]
    fn powershell_cmdlets_are_not_native_commands() {
        let spec = crate::job::CommandSpec {
            program: "Write-Output".to_owned(),
            args: Vec::new(),
            working_directory: std::env::current_dir().expect("current directory"),
        };

        assert!(!super::is_available(&spec));
    }
}
