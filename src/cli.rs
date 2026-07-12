use std::{
    fs,
    io::Write,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use fs2::FileExt;

use crate::{
    command,
    config::{
        self, Config, ProcessAction, ProcessRule, Tool, ToolBackground, ToolProbe, UserConfig,
        UserTool,
    },
    datetime, detach, doctor,
    error::{Error, Result},
    job::{Job, JobStatus, JobStore},
    process,
    state::StateDirs,
    worker,
};

/// Lock-aware, cross-platform toolchain updater.
#[derive(Debug, Parser)]
#[command(version, about, subcommand_precedence_over_arg = true)]
pub struct Cli {
    /// Override the per-user directory used for jobs and logs.
    #[arg(long, global = true, env = "DVUP_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// Open an existing TOML file directly in the interactive editor.
    #[arg(value_name = "TOML", value_parser = parse_toml_file)]
    toml_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

fn parse_toml_file(value: &str) -> std::result::Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
    {
        return Err("direct editor input must be a .toml file".to_owned());
    }
    if !path.is_file() {
        return Err(format!("TOML file does not exist: {}", path.display()));
    }
    Ok(path)
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Launch the interactive terminal interface.
    Tui {
        /// Explicit user manifest layered on built-ins; otherwise use global dvup_custom.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Add a user-level custom update command.
    Add {
        /// Name used by `dvup update <name>`.
        name: String,
        /// Replace an existing custom or built-in tool with the same name.
        #[arg(long)]
        force: bool,
        /// Command and arguments, for example: brew upgrade ripgrep.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Atomically edit or rename a user-level custom update command.
    #[command(hide = true)]
    Edit {
        original_name: String,
        name: String,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Remove a user-level custom update command.
    Remove { name: String },

    /// Create a clean global user manifest without copying built-ins.
    Init {
        /// Write to an explicit path instead of the global dvup_custom.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },

    /// Update all installed tools, or one named tool when provided.
    Update {
        /// Built-in or configured tool name; omit it to update all tools.
        #[arg(conflicts_with = "all")]
        tool: Option<String>,
        /// Explicit alias for the default all-tools behavior.
        #[arg(long)]
        all: bool,
        /// Extra arguments appended to the tool command.
        #[arg(conflicts_with = "all")]
        extra_args: Vec<String>,
        /// Explicit user manifest layered on built-ins; otherwise use global dvup_custom.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        #[command(flatten)]
        execution: ExecutionOptions,
    },

    /// Update dvup itself from crates.io in a detached worker.
    SelfUpdate {
        /// Reinstall even when the installed version is already current.
        #[arg(long)]
        force: bool,
    },

    /// Show built-in/configured tools and whether they are installed.
    List {
        /// Explicit user manifest layered on built-ins; otherwise use global dvup_custom.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Diagnose duplicate installations and PATH/version conflicts.
    Doctor {
        /// Inspect only one configured tool.
        tool: Option<String>,
        /// Explicit user manifest layered on built-ins; otherwise use global dvup_custom.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Run an arbitrary update command with optional process-lock awareness.
    Run {
        /// Friendly name shown in the job list.
        #[arg(long, default_value = "ad-hoc")]
        name: String,
        /// Process name that must exit before the command can run. Repeatable.
        #[arg(long = "wait-for")]
        wait_for: Vec<String>,
        /// Process policy in `ACTION:NAME[:COMMAND_CONTAINS]` form. Repeatable.
        #[arg(long = "process-rule", value_name = "ACTION:NAME[:COMMAND_CONTAINS]")]
        process_rules: Vec<ProcessRule>,
        #[arg(long, default_value_t = 86_400)]
        lock_timeout_secs: u64,
        #[arg(long, default_value_t = 8)]
        retries: u32,
        #[arg(long, default_value_t = 2)]
        retry_delay_secs: u64,
        #[command(flatten)]
        execution: ExecutionOptions,
        /// Program and arguments to execute, following `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },

    /// List jobs or show one job and its log.
    Jobs {
        job_id: Option<String>,
        #[arg(long)]
        log: bool,
    },

    #[command(name = "__worker", hide = true)]
    Worker { job_id: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum BackgroundMode {
    /// Defer only when a configured process is active or the command reports a lock.
    Auto,
    /// Always execute in a detached worker.
    Always,
    /// Never defer; report active locks as an error.
    Never,
}

#[derive(Debug, Args)]
struct ExecutionOptions {
    #[arg(long, value_enum, default_value_t = BackgroundMode::Auto)]
    background: BackgroundMode,
    /// Convert safe wait rules to terminate rules for this invocation.
    #[arg(long)]
    terminate_locking_processes: bool,
}

pub fn run(cli: Cli) -> Result<u8> {
    let state = match cli.state_dir {
        Some(root) => StateDirs::at_runtime(root),
        None => StateDirs::discover()?,
    };

    match cli.command {
        None => crate::tui::run(state, None, cli.toml_file),
        Some(Commands::Tui { config }) => crate::tui::run(state, config, None),
        Some(Commands::Add {
            name,
            force,
            command,
        }) => add_custom_tool(state, name, command, force),
        Some(Commands::Edit {
            original_name,
            name,
            command,
        }) => edit_custom_tool(state, &original_name, name, command),
        Some(Commands::Remove { name }) => remove_custom_tool(state, &name),
        Some(Commands::Init { config, force }) => init(&state, config, force),
        Some(Commands::Update {
            tool,
            all,
            extra_args,
            config,
            execution,
        }) => {
            let (manifest, working_directory, _) = load_manifest(config, &state)?;
            if should_update_all(all, &tool) {
                return update_all(
                    manifest,
                    working_directory,
                    execution.background,
                    execution.terminate_locking_processes,
                    state,
                );
            }
            let tool = tool.expect("tool is present after all-tools branch");
            let mut definition = manifest
                .tools
                .get(&tool)
                .cloned()
                .ok_or_else(|| Error::ToolNotFound(tool.clone()))?;
            if !definition.supports_current_platform() {
                return Err(Error::Message(format!(
                    "tool `{tool}` is not enabled on {}",
                    std::env::consts::OS
                )));
            }
            ensure_tool_ready(&tool, &definition, &working_directory)?;
            let background = effective_background(execution.background, definition.background);
            definition.args.extend(extra_args);
            let mut job = Job::from_tool(tool, definition, working_directory);
            if execution.terminate_locking_processes {
                job.terminate_waiting_processes()?;
            }
            execute(job, background, state)
        }
        Some(Commands::SelfUpdate { force }) => {
            let mut definition = Config::starter()
                .tools
                .remove("dvup")
                .expect("bundled dvup self-update preset");
            if force {
                definition.args.push("--force".to_owned());
            }
            let working_directory = std::env::current_dir()?;
            ensure_tool_ready("dvup", &definition, &working_directory)?;
            let job = Job::from_tool("dvup".to_owned(), definition, working_directory);
            execute(job, BackgroundMode::Always, state)
        }
        Some(Commands::List { config }) => {
            let (manifest, working_directory, source) = load_manifest(config, &state)?;
            list_tools(&manifest, &working_directory, source)
        }
        Some(Commands::Doctor { tool, config }) => {
            let (manifest, working_directory, _) = load_manifest(config, &state)?;
            doctor::run(&manifest, &working_directory, tool.as_deref())
        }
        Some(Commands::Run {
            name,
            wait_for,
            process_rules,
            lock_timeout_secs,
            retries,
            retry_delay_secs,
            execution,
            command,
        }) => {
            let Some((program, args)) = command.split_first() else {
                return Err(Error::EmptyCommand);
            };
            let mut processes = process_rules;
            processes.extend(wait_for.into_iter().map(ProcessRule::wait));
            let definition = Tool {
                program: program.clone(),
                args: args.to_vec(),
                probe: ToolProbe {
                    program: name.clone(),
                    args: vec!["--version".to_owned()],
                },
                background: ToolBackground::Auto,
                processes,
                lock_timeout_secs,
                retries,
                retry_delay_secs,
                platforms: Vec::new(),
                resource_group: None,
            };
            let mut job = Job::from_tool(name, definition, std::env::current_dir()?);
            if execution.terminate_locking_processes {
                job.terminate_waiting_processes()?;
            }
            execute(job, execution.background, state)
        }
        Some(Commands::Jobs { job_id, log }) => show_jobs(state, job_id, log),
        Some(Commands::Worker { job_id }) => {
            let store = JobStore::new(state)?;
            worker::run(&job_id, &store)?;
            Ok(0)
        }
    }
}

fn should_update_all(explicit_all: bool, tool: &Option<String>) -> bool {
    explicit_all || tool.is_none()
}

fn ensure_tool_ready(name: &str, tool: &Tool, working_directory: &std::path::Path) -> Result<()> {
    match command::tool_readiness(tool, working_directory) {
        command::ToolReadiness::Installed => Ok(()),
        command::ToolReadiness::TargetMissing => Err(Error::Message(format!(
            "tool `{name}` is not installed or `{}` is not on PATH",
            tool.probe.program
        ))),
        command::ToolReadiness::UpdaterMissing => Err(Error::Message(format!(
            "tool `{name}` cannot be updated because `{}` is not installed or not on PATH",
            tool.program
        ))),
        command::ToolReadiness::Unsupported => Err(Error::Message(format!(
            "tool `{name}` is not enabled on {}",
            std::env::consts::OS
        ))),
    }
}

fn effective_background(requested: BackgroundMode, configured: ToolBackground) -> BackgroundMode {
    match configured {
        ToolBackground::Auto => requested,
        ToolBackground::Always => BackgroundMode::Always,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ManifestSource {
    BuiltIn,
    Customized,
    Explicit,
}

pub(crate) fn load_manifest(
    config_path: Option<PathBuf>,
    state: &StateDirs,
) -> Result<(Config, PathBuf, ManifestSource)> {
    if let Some(path) = config_path {
        let working_directory = config_working_directory(&path)?;
        let mut manifest = Config::starter();
        manifest
            .tools
            .extend(UserConfig::load(&path)?.resolve()?.tools);
        manifest.validate()?;
        return Ok((manifest, working_directory, ManifestSource::Explicit));
    }

    let mut manifest = Config::starter();
    let mut customized = false;
    let custom_path = state.custom_config_path();
    if custom_path.is_file() {
        manifest
            .tools
            .extend(UserConfig::load(&custom_path)?.resolve()?.tools);
        customized = true;
    }

    manifest.validate()?;
    let working_directory = if customized {
        config_working_directory(&custom_path)?
    } else {
        std::env::current_dir()?
    };
    Ok((
        manifest,
        working_directory,
        if customized {
            ManifestSource::Customized
        } else {
            ManifestSource::BuiltIn
        },
    ))
}

fn list_tools(
    manifest: &Config,
    working_directory: &std::path::Path,
    source: ManifestSource,
) -> Result<u8> {
    let source = match source {
        ManifestSource::BuiltIn => "built-in presets",
        ManifestSource::Customized => "built-in presets + user customization",
        ManifestSource::Explicit => "built-in presets + explicit user manifest",
    };
    println!("source: {source}\n");
    println!("{:<18} {:<12} COMMAND", "TOOL", "STATUS");
    for (name, tool) in &manifest.tools {
        let update_command = command::update_spec(tool, working_directory);
        let status = match command::tool_readiness(tool, working_directory) {
            command::ToolReadiness::Unsupported => "unsupported",
            command::ToolReadiness::TargetMissing => "missing",
            command::ToolReadiness::UpdaterMissing => "no updater",
            command::ToolReadiness::Installed => "installed",
        };
        let actual_command = format_command(&update_command);
        println!(
            "{name:<18} {status:<12} {}",
            display_command(name, &actual_command)
        );
    }
    Ok(0)
}

fn add_custom_tool(
    state: StateDirs,
    name: String,
    command: Vec<String>,
    force: bool,
) -> Result<u8> {
    let Some((program, args)) = command.split_first() else {
        return Err(Error::EmptyCommand);
    };
    state.ensure()?;
    let path = state.custom_config_path();
    let mut custom = if path.is_file() {
        UserConfig::load(&path)?
    } else {
        UserConfig::empty()
    };
    let conflicts = custom.tools.contains_key(&name) || Config::starter().tools.contains_key(&name);
    if conflicts && !force {
        return Err(Error::Message(format!(
            "tool `{name}` already exists; pass --force before the tool name to replace it"
        )));
    }

    custom.tools.insert(
        name.clone(),
        UserTool::custom(&name, program.clone(), args.to_vec()),
    );
    custom.save(&path)?;
    println!("added {name}: {}", command.join(" "));
    println!("stored: {}", path.display());
    println!("run: dvup update {name}");
    Ok(0)
}

fn edit_custom_tool(
    state: StateDirs,
    original_name: &str,
    name: String,
    command: Vec<String>,
) -> Result<u8> {
    let Some((program, args)) = command.split_first() else {
        return Err(Error::EmptyCommand);
    };
    let path = state.custom_config_path();
    if !path.is_file() {
        return Err(Error::Message(format!(
            "custom tool `{original_name}` does not exist"
        )));
    }
    let mut custom = UserConfig::load(&path)?;
    if !custom.tools.contains_key(original_name) {
        return Err(Error::Message(format!(
            "custom tool `{original_name}` does not exist"
        )));
    }
    if name != original_name
        && (custom.tools.contains_key(&name) || Config::starter().tools.contains_key(&name))
    {
        return Err(Error::Message(format!("tool `{name}` already exists")));
    }

    custom.tools.remove(original_name);
    custom.tools.insert(
        name.clone(),
        UserTool::custom(&name, program.clone(), args.to_vec()),
    );
    custom.save(&path)?;
    if name == original_name {
        println!("edited {name}: {}", command.join(" "));
    } else {
        println!("renamed {original_name} to {name}: {}", command.join(" "));
    }
    println!("stored: {}", path.display());
    println!("run: dvup update {name}");
    Ok(0)
}

fn remove_custom_tool(state: StateDirs, name: &str) -> Result<u8> {
    let path = state.custom_config_path();
    if !path.is_file() {
        return Err(Error::Message(format!(
            "custom tool `{name}` does not exist"
        )));
    }
    let mut custom = UserConfig::load(&path)?;
    if custom.tools.remove(name).is_none() {
        return Err(Error::Message(format!(
            "custom tool `{name}` does not exist"
        )));
    }
    if custom.tools.is_empty() {
        fs::remove_file(path)?;
    } else {
        custom.save(&path)?;
    }
    println!("removed {name}");
    Ok(0)
}

#[derive(Debug)]
enum ExecutionKind {
    Updated,
    Queued { job_id: String, reason: String },
}

#[derive(Debug)]
struct ExecutionSuccess {
    kind: ExecutionKind,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
struct ExecutionFailure {
    message: String,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ExecutionFailure {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn from_error(error: impl std::fmt::Display) -> Self {
        Self::message(error.to_string())
    }
}

type ExecutionResult = std::result::Result<ExecutionSuccess, ExecutionFailure>;

#[derive(Debug)]
enum BatchStatus {
    Updated,
    Queued { job_id: String, reason: String },
    Skipped { reason: String },
    Failed(ExecutionFailure),
}

#[derive(Debug)]
struct BatchReport {
    index: usize,
    name: String,
    command: String,
    resource_group: String,
    elapsed: Duration,
    status: BatchStatus,
}

fn update_all(
    manifest: Config,
    working_directory: PathBuf,
    mode: BackgroundMode,
    terminate_locking_processes: bool,
    state: StateDirs,
) -> Result<u8> {
    let started = Instant::now();
    let total = manifest.tools.len();
    let mut reports = Vec::new();
    let mut candidates = Vec::new();

    let store = JobStore::new(state.clone())?;
    detach::cleanup_workers(store.dirs())?;

    for (index, (name, tool)) in manifest.tools.into_iter().enumerate() {
        let readiness = command::tool_readiness(&tool, &working_directory);
        let probe_program = tool.probe.program.clone();
        let tool_mode = effective_background(mode, tool.background);
        let mut job = Job::from_tool(name.clone(), tool, working_directory.clone());
        if terminate_locking_processes {
            job.terminate_waiting_processes()?;
        }
        let command_text = format_command(&job.command);
        if readiness == command::ToolReadiness::Unsupported {
            reports.push(BatchReport {
                index,
                name,
                command: command_text,
                resource_group: job.resource_group,
                elapsed: Duration::ZERO,
                status: BatchStatus::Skipped {
                    reason: format!("not supported on {}", std::env::consts::OS),
                },
            });
            continue;
        }
        if readiness == command::ToolReadiness::TargetMissing {
            reports.push(BatchReport {
                index,
                name,
                command: command_text,
                resource_group: job.resource_group,
                elapsed: Duration::ZERO,
                status: BatchStatus::Skipped {
                    reason: format!("`{probe_program}` is not installed or not on PATH"),
                },
            });
            continue;
        }
        if readiness == command::ToolReadiness::UpdaterMissing {
            reports.push(BatchReport {
                index,
                name,
                command: command_text,
                resource_group: job.resource_group,
                elapsed: Duration::ZERO,
                status: BatchStatus::Skipped {
                    reason: format!(
                        "update program `{}` is not installed or not on PATH",
                        job.command.program
                    ),
                },
            });
            continue;
        }
        candidates.push((index, name, command_text, job, tool_mode));
    }

    println!(
        "updating {} installed tool(s) in parallel ({} configured)...",
        candidates.len(),
        total
    );

    let mut handles = Vec::new();
    for (index, name, command_text, job, tool_mode) in candidates {
        let state = state.clone();
        let thread_name = format!("update-{name}");
        let resource_group = job.resource_group.clone();
        let error_name = name.clone();
        let error_command = command_text.clone();
        let error_resource_group = resource_group.clone();
        let handle = thread::Builder::new().name(thread_name).spawn(move || {
            let started = Instant::now();
            let result = execute_inner(job, tool_mode, state);
            let status = match result {
                Ok(success) => match success.kind {
                    ExecutionKind::Updated => BatchStatus::Updated,
                    ExecutionKind::Queued { job_id, reason } => {
                        BatchStatus::Queued { job_id, reason }
                    }
                },
                Err(failure) => BatchStatus::Failed(failure),
            };
            BatchReport {
                index,
                name,
                command: command_text,
                resource_group,
                elapsed: started.elapsed(),
                status,
            }
        });
        match handle {
            Ok(handle) => handles.push((
                index,
                error_name,
                error_command,
                error_resource_group,
                handle,
            )),
            Err(error) => reports.push(BatchReport {
                index,
                name: error_name,
                command: error_command,
                resource_group: error_resource_group,
                elapsed: Duration::ZERO,
                status: BatchStatus::Failed(ExecutionFailure::from_error(format!(
                    "failed to start update thread: {error}"
                ))),
            }),
        }
    }

    for (index, name, command, resource_group, handle) in handles {
        match handle.join() {
            Ok(report) => reports.push(report),
            Err(_) => reports.push(BatchReport {
                index,
                name,
                command,
                resource_group,
                elapsed: Duration::ZERO,
                status: BatchStatus::Failed(ExecutionFailure::message(
                    "update thread panicked unexpectedly",
                )),
            }),
        }
    }

    reports.sort_by_key(|report| report.index);
    Ok(render_batch_report(&reports, started.elapsed()))
}

fn init(state: &StateDirs, path: Option<PathBuf>, force: bool) -> Result<u8> {
    let path = path.unwrap_or_else(|| state.custom_config_path());
    if path.exists() && !force {
        return Err(Error::FileExists(path));
    }
    let template = config::USER_TEMPLATE;
    let _validated_template = UserConfig::parse(template)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, template)?;
    println!("created {}", path.display());
    Ok(0)
}

fn execute(job: Job, mode: BackgroundMode, state: StateDirs) -> Result<u8> {
    let store = JobStore::new(state.clone())?;
    detach::cleanup_workers(store.dirs())?;
    let name = job.name.clone();
    let command_text = format_command(&job.command);
    let command_display = display_command(&name, &command_text);
    match execute_inner(job, mode, state) {
        Ok(success) => {
            write_captured_output(&success.stdout, &success.stderr)?;
            match success.kind {
                ExecutionKind::Updated => println!("updated {name}: {command_display}"),
                ExecutionKind::Queued { job_id, reason } => {
                    println!("queued {name}: {reason}");
                    println!("job: {job_id}");
                    println!("inspect: dvup jobs {job_id} --log");
                }
            }
            Ok(0)
        }
        Err(failure) => {
            write_captured_output(&failure.stdout, &failure.stderr)?;
            let exit = failure
                .exit_code
                .map(|code| format!(" (exit code {code})"))
                .unwrap_or_default();
            Err(Error::Message(format!(
                "{name} failed{exit}: {}\ncommand: {command_text}",
                failure.message
            )))
        }
    }
}

fn execute_inner(job: Job, mode: BackgroundMode, state: StateDirs) -> ExecutionResult {
    let store = JobStore::new(state).map_err(ExecutionFailure::from_error)?;
    let matches = process::find_matching_processes(&job.process_rules);
    let rejected = matches
        .iter()
        .filter(|matched| matched.action == ProcessAction::Fail)
        .collect::<Vec<_>>();

    if !rejected.is_empty() {
        return Err(ExecutionFailure::message(format!(
            "process policy rejected the update because {} is running",
            format_matches(&rejected)
        )));
    }

    match mode {
        BackgroundMode::Always => schedule_inner(job, &store, "background execution requested"),
        BackgroundMode::Auto if !matches.is_empty() => {
            let action = if matches
                .iter()
                .any(|matched| matched.action == ProcessAction::Terminate)
            {
                "applying terminate process policy"
            } else {
                "waiting on process policy"
            };
            let reason = format!("{action}: {}", format_matches(&matches));
            schedule_inner(job, &store, &reason)
        }
        BackgroundMode::Never if !matches.is_empty() => Err(ExecutionFailure::message(format!(
            "update is blocked by {}; process policies require a background worker, so use --background auto",
            format_matches(&matches)
        ))),
        BackgroundMode::Auto | BackgroundMode::Never => run_now_inner(job, mode, &store),
    }
}

fn run_now_inner(job: Job, mode: BackgroundMode, store: &JobStore) -> ExecutionResult {
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.dirs().resource_lock_path(&job.resource_group))
        .map_err(ExecutionFailure::from_error)?;
    lock_file
        .lock_exclusive()
        .map_err(ExecutionFailure::from_error)?;

    let result = command::run(&job.command);
    FileExt::unlock(&lock_file).map_err(ExecutionFailure::from_error)?;
    let result = result.map_err(ExecutionFailure::from_error)?;

    if result.status.success() {
        return Ok(ExecutionSuccess {
            kind: ExecutionKind::Updated,
            stdout: result.stdout,
            stderr: result.stderr,
        });
    }
    if matches!(mode, BackgroundMode::Auto) && result.is_lock_failure() {
        let mut scheduled =
            schedule_inner(job, store, "the update command reported a locked file")?;
        scheduled.stdout = result.stdout;
        scheduled.stderr = result.stderr;
        return Ok(scheduled);
    }
    if result.is_permission_failure() && !result.is_lock_failure() {
        return Err(ExecutionFailure {
            message: "permission denied; configure a user-owned global package prefix or run from an elevated terminal".to_owned(),
            exit_code: result.exit_code(),
            stdout: result.stdout,
            stderr: result.stderr,
        });
    }
    Err(ExecutionFailure {
        message: "update command returned a non-zero exit status".to_owned(),
        exit_code: result.exit_code(),
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

fn schedule_inner(mut job: Job, store: &JobStore, reason: &str) -> ExecutionResult {
    job.set_status(JobStatus::Pending);
    store.save(&job).map_err(ExecutionFailure::from_error)?;
    if let Err(error) = detach::spawn_worker(&job, store.dirs()) {
        job.set_status(JobStatus::Failed {
            message: format!("failed to start background worker: {error}"),
            exit_code: None,
        });
        if let Err(save_error) = store.save(&job) {
            return Err(ExecutionFailure::message(format!(
                "failed to start background worker: {error}; also failed to save job state: {save_error}"
            )));
        }
        return Err(ExecutionFailure::from_error(error));
    }
    Ok(ExecutionSuccess {
        kind: ExecutionKind::Queued {
            job_id: job.id,
            reason: reason.to_owned(),
        },
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

fn write_captured_output(stdout: &[u8], stderr: &[u8]) -> Result<()> {
    std::io::stdout().write_all(stdout)?;
    std::io::stdout().flush()?;
    std::io::stderr().write_all(stderr)?;
    std::io::stderr().flush()?;
    Ok(())
}

fn render_batch_report(reports: &[BatchReport], elapsed: Duration) -> u8 {
    println!("\nRESULTS");
    println!("{:<10} {:<18} {:>8}  DETAIL", "STATUS", "TOOL", "TIME");

    let mut updated = 0_usize;
    let mut queued = 0_usize;
    let mut skipped = 0_usize;
    let mut failed = 0_usize;

    for report in reports {
        let (status, detail) = match &report.status {
            BatchStatus::Updated => {
                updated += 1;
                ("UPDATED", display_command(&report.name, &report.command))
            }
            BatchStatus::Queued { job_id, reason } => {
                queued += 1;
                ("QUEUED", format!("job {job_id}: {reason}"))
            }
            BatchStatus::Skipped { reason } => {
                skipped += 1;
                ("SKIPPED", reason.clone())
            }
            BatchStatus::Failed(failure) => {
                failed += 1;
                ("FAILED", failure.message.clone())
            }
        };
        println!(
            "{status:<10} {:<18} {:>8}  {detail}",
            report.name,
            format_duration(report.elapsed)
        );
    }

    for report in reports {
        let BatchStatus::Failed(failure) = &report.status else {
            continue;
        };
        println!("\nFAILURE: {}", report.name);
        println!("  command:  {}", report.command);
        println!("  resource: {}", report.resource_group);
        println!("  reason:   {}", failure.message);
        if let Some(exit_code) = failure.exit_code {
            println!("  exit:     {exit_code}");
        }
        print_output_excerpt("stdout", &failure.stdout);
        print_output_excerpt("stderr", &failure.stderr);
    }

    if queued > 0 {
        println!("\nQueued jobs continue in the background. Inspect them with `dvup jobs`.");
    }
    println!(
        "\nSUMMARY: {updated} updated, {queued} queued, {skipped} skipped, {failed} failed in {}",
        format_duration(elapsed)
    );

    u8::from(failed > 0)
}

fn print_output_excerpt(label: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    const MAX_LINES: usize = 30;
    const MAX_BYTES: usize = 8_192;
    let start = bytes.len().saturating_sub(MAX_BYTES);
    let text = String::from_utf8_lossy(&bytes[start..]);
    let lines: Vec<_> = text.lines().collect();
    let first_line = lines.len().saturating_sub(MAX_LINES);
    let clipped = start > 0 || first_line > 0;
    println!("  {label}:{}", if clipped { " (last part)" } else { "" });
    for line in &lines[first_line..] {
        println!("    {line}");
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 10 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

fn format_command(command: &crate::job::CommandSpec) -> String {
    std::iter::once(command.program.as_str())
        .chain(command.args.iter().map(String::as_str))
        .map(|argument| {
            if argument.contains(char::is_whitespace) {
                format!("\"{}\"", argument.replace('"', "\\\""))
            } else {
                argument.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns a concise command label for routine user-facing output.
///
/// Full command text remains available in failure diagnostics and job details.
pub(crate) fn display_command(name: &str, actual_command: &str) -> String {
    if name.eq_ignore_ascii_case("bun") && actual_command.contains("bun.sh/install") {
        "Bun official installer".to_owned()
    } else if name.eq_ignore_ascii_case("uv") && actual_command.contains("astral.sh/uv/install") {
        "uv official installer".to_owned()
    } else {
        actual_command.to_owned()
    }
}

fn show_jobs(state: StateDirs, job_id: Option<String>, include_log: bool) -> Result<u8> {
    let store = JobStore::new(state)?;
    detach::cleanup_workers(store.dirs())?;
    if let Some(id) = job_id {
        let job = store.load(&id)?;
        println!("job:     {}", job.id);
        println!("tool:    {}", job.name);
        println!("status:  {}", job.status.label());
        println!(
            "created: {}",
            datetime::format_unix_ms(job.created_at_unix_ms)
        );
        println!(
            "updated: {}",
            datetime::format_unix_ms(job.updated_at_unix_ms)
        );
        match &job.status {
            JobStatus::WaitingForLocks { processes } => {
                println!("waiting: {}", format_locks(processes));
            }
            JobStatus::TerminatingProcesses { processes } => {
                println!("stopping: {}", format_locks(processes));
            }
            JobStatus::Running { attempt } => println!("attempt: {attempt}"),
            JobStatus::Failed { message, exit_code } => {
                println!("error:   {message}");
                if let Some(code) = exit_code {
                    println!("exit:    {code}");
                }
            }
            JobStatus::Pending | JobStatus::Succeeded { .. } => {}
        }
        println!(
            "command: {} {}",
            job.command.program,
            job.command.args.join(" ")
        );
        if include_log {
            let log = store.read_log(&id)?;
            if !log.is_empty() {
                println!("\n--- log ---");
                print!("{}", String::from_utf8_lossy(&log));
            }
        }
        return Ok(0);
    }

    let jobs = store.list()?;
    if jobs.is_empty() {
        println!("no jobs");
        return Ok(0);
    }
    println!(
        "{:<19} {:<34} {:<22} {:<20}",
        "UPDATED", "JOB", "TOOL", "STATUS"
    );
    for job in jobs {
        println!(
            "{:<19} {:<34} {:<22} {:<20}",
            datetime::format_unix_ms(job.updated_at_unix_ms),
            job.id,
            job.name,
            job.status.label()
        );
    }
    Ok(0)
}

fn format_locks(locks: &[crate::job::LockingProcess]) -> String {
    locks
        .iter()
        .map(|process| format!("{} ({})", process.name, process.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_matches(matches: &[impl std::borrow::Borrow<process::MatchedProcess>]) -> String {
    matches
        .iter()
        .map(|matched| {
            let matched = matched.borrow();
            format!(
                "{} ({}, {})",
                matched.process.name, matched.process.pid, matched.action
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn config_working_directory(path: &std::path::Path) -> Result<PathBuf> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent.to_path_buf()),
        _ => Ok(std::env::current_dir()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_the_bun_official_installer_a_friendly_display_name() {
        assert_eq!(
            display_command(
                "bun",
                "Invoke-Expression \"Invoke-RestMethod https://bun.sh/install.ps1 | Invoke-Expression\"",
            ),
            "Bun official installer"
        );
        assert_eq!(
            display_command(
                "bun",
                "bash -c \"curl -fsSL https://bun.sh/install | bash\"",
            ),
            "Bun official installer"
        );
    }

    #[test]
    fn gives_the_uv_official_installer_a_friendly_display_name() {
        assert_eq!(
            display_command(
                "uv",
                "powershell -ExecutionPolicy ByPass -c \"irm https://astral.sh/uv/install.ps1 | iex\"",
            ),
            "uv official installer"
        );
        assert_eq!(
            display_command(
                "uv",
                "sh -c \"curl -LsSf https://astral.sh/uv/install.sh | sh\"",
            ),
            "uv official installer"
        );
    }

    #[test]
    fn keeps_regular_commands_visible() {
        assert_eq!(display_command("rustup", "rustup update"), "rustup update");
    }

    #[test]
    fn relative_config_uses_current_directory() {
        assert_eq!(
            config_working_directory(std::path::Path::new("dvup_custom.toml"))
                .expect("working directory"),
            std::env::current_dir().expect("current directory")
        );
    }

    #[test]
    fn nested_config_uses_its_parent() {
        assert_eq!(
            config_working_directory(std::path::Path::new("config/tools.toml"))
                .expect("working directory"),
            PathBuf::from("config")
        );
    }

    #[test]
    fn parses_update_all() {
        let cli = Cli::try_parse_from(["dvup", "update", "--all"]).expect("parse --all");

        assert!(matches!(
            cli.command,
            Some(Commands::Update { all: true, .. })
        ));
    }

    #[test]
    fn parses_self_update_and_force() {
        let regular = Cli::try_parse_from(["dvup", "self-update"]).expect("parse self-update");
        assert!(matches!(
            regular.command,
            Some(Commands::SelfUpdate { force: false })
        ));

        let forced =
            Cli::try_parse_from(["dvup", "self-update", "--force"]).expect("parse --force");
        assert!(matches!(
            forced.command,
            Some(Commands::SelfUpdate { force: true })
        ));
    }

    #[test]
    fn configured_background_always_overrides_the_requested_mode() {
        assert_eq!(
            effective_background(BackgroundMode::Never, ToolBackground::Always),
            BackgroundMode::Always
        );
        assert_eq!(
            effective_background(BackgroundMode::Never, ToolBackground::Auto),
            BackgroundMode::Never
        );
    }

    #[test]
    fn parses_terminate_locking_processes_policy() {
        let cli = Cli::try_parse_from(["dvup", "update", "codex", "--terminate-locking-processes"])
            .expect("parse terminate policy");

        match cli.command {
            Some(Commands::Update { execution, .. }) => {
                assert!(execution.terminate_locking_processes);
            }
            _ => panic!("expected update command"),
        }
    }

    #[test]
    fn parses_run_wait_for_processes() {
        let cli = Cli::try_parse_from([
            "dvup",
            "run",
            "--wait-for",
            "example",
            "--",
            "updater",
            "update",
        ])
        .expect("parse run wait rule");

        assert!(matches!(
            cli.command,
            Some(Commands::Run { wait_for, command, .. })
                if wait_for == ["example"] && command == ["updater", "update"]
        ));
    }

    #[test]
    fn rejects_tool_name_with_update_all() {
        let error = Cli::try_parse_from(["dvup", "update", "codex", "--all"])
            .expect_err("tool and --all should conflict");

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_extra_tool_arguments_after_separator() {
        let cli = Cli::try_parse_from(["dvup", "update", "scoop", "--", "zedg", "git"])
            .expect("parse extra arguments");

        match cli.command {
            Some(Commands::Update {
                tool,
                all,
                extra_args,
                ..
            }) => {
                assert_eq!(tool.as_deref(), Some("scoop"));
                assert!(!all);
                assert_eq!(extra_args, ["zedg", "git"]);
            }
            _ => panic!("expected update command"),
        }
    }

    #[test]
    fn parses_simple_extra_tool_arguments_without_separator() {
        let cli = Cli::try_parse_from(["dvup", "update", "scoop", "zedg", "git"])
            .expect("parse simple extra arguments");

        match cli.command {
            Some(Commands::Update {
                tool, extra_args, ..
            }) => {
                assert_eq!(tool.as_deref(), Some("scoop"));
                assert_eq!(extra_args, ["zedg", "git"]);
            }
            _ => panic!("expected update command"),
        }
    }

    #[test]
    fn update_without_tool_means_all_tools() {
        let cli = Cli::try_parse_from(["dvup", "update"]).expect("parse update");

        match cli.command {
            Some(Commands::Update { tool, all, .. }) => assert!(should_update_all(all, &tool)),
            _ => panic!("expected update command"),
        }
    }

    #[test]
    fn parses_list_command() {
        let cli = Cli::try_parse_from(["dvup", "list"]).expect("parse list");

        assert!(matches!(cli.command, Some(Commands::List { config: None })));
    }

    #[test]
    fn parses_doctor_for_all_or_one_tool() {
        let all = Cli::try_parse_from(["dvup", "doctor"]).expect("parse doctor");
        assert!(matches!(
            all.command,
            Some(Commands::Doctor {
                tool: None,
                config: None
            })
        ));

        let one = Cli::try_parse_from(["dvup", "doctor", "codex"]).expect("parse doctor tool");
        assert!(matches!(
            one.command,
            Some(Commands::Doctor {
                tool: Some(tool),
                config: None
            }) if tool == "codex"
        ));
    }

    #[test]
    fn rejects_extra_arguments_with_update_all() {
        let error = Cli::try_parse_from(["dvup", "update", "--all", "--", "zedg"])
            .expect_err("extra arguments and --all should conflict");

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_simple_custom_command() {
        let cli = Cli::try_parse_from(["dvup", "add", "claude", "claude", "install"])
            .expect("parse custom command");

        match cli.command {
            Some(Commands::Add {
                name,
                force,
                command,
            }) => {
                assert_eq!(name, "claude");
                assert!(!force);
                assert_eq!(command, ["claude", "install"]);
            }
            _ => panic!("expected add command"),
        }
    }

    #[test]
    fn parses_homebrew_package_upgrade_command() {
        let cli = Cli::try_parse_from(["dvup", "add", "ripgrep", "brew", "upgrade", "ripgrep"])
            .expect("parse Homebrew package command");

        match cli.command {
            Some(Commands::Add { name, command, .. }) => {
                assert_eq!(name, "ripgrep");
                assert_eq!(command, ["brew", "upgrade", "ripgrep"]);
            }
            _ => panic!("expected add command"),
        }
    }

    #[test]
    fn no_subcommand_opens_tui() {
        let cli = Cli::try_parse_from(["dvup"]).expect("parse TUI default");

        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_explicit_tui_command() {
        let cli = Cli::try_parse_from(["dvup", "tui"]).expect("parse TUI command");

        assert!(matches!(cli.command, Some(Commands::Tui { config: None })));
    }

    #[test]
    fn parses_an_existing_toml_file_as_a_direct_editor_target() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("broken.toml");
        std::fs::write(&path, "invalid =").expect("write TOML");

        let cli = Cli::try_parse_from([std::ffi::OsStr::new("dvup"), path.as_os_str()])
            .expect("parse direct TOML editor target");

        assert!(cli.command.is_none());
        assert_eq!(cli.toml_file.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn direct_editor_target_requires_an_existing_toml_file() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let text = temporary.path().join("notes.txt");
        std::fs::write(&text, "text").expect("write text file");
        let missing = temporary.path().join("missing.toml");

        assert!(Cli::try_parse_from([std::ffi::OsStr::new("dvup"), text.as_os_str()]).is_err());
        assert!(Cli::try_parse_from([std::ffi::OsStr::new("dvup"), missing.as_os_str()]).is_err());
    }

    #[test]
    fn global_state_directory_still_combines_with_subcommands() {
        let cli = Cli::try_parse_from(["dvup", "--state-dir", "state", "list"])
            .expect("parse global state directory with subcommand");

        assert_eq!(
            cli.state_dir.as_deref(),
            Some(std::path::Path::new("state"))
        );
        assert!(matches!(cli.command, Some(Commands::List { config: None })));
        assert!(cli.toml_file.is_none());
    }

    #[test]
    fn adds_merges_and_removes_user_tool() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));

        add_custom_tool(
            state.clone(),
            "claude".to_owned(),
            vec!["claude".to_owned(), "install".to_owned()],
            false,
        )
        .expect("add custom tool");
        let stored = fs::read_to_string(state.custom_config_path()).expect("stored user manifest");
        assert!(stored.contains("update = [\"claude\", \"install\"]"));
        assert!(stored.contains("probe = [\"claude\", \"--version\"]"));
        assert!(!stored.contains("program ="));
        assert!(!stored.contains("background ="));
        let (manifest, _, source) = load_manifest(None, &state).expect("merged manifest");
        let claude = manifest.tools.get("claude").expect("custom tool");
        assert_eq!(claude.program, "claude");
        assert_eq!(claude.args, ["install"]);
        assert_eq!(claude.processes.len(), 1);
        assert_eq!(claude.processes[0].name, "claude");
        assert_eq!(claude.processes[0].action, ProcessAction::Wait);
        assert!(matches!(source, ManifestSource::Customized));

        add_custom_tool(
            state.clone(),
            "claude".to_owned(),
            vec![
                "claude".to_owned(),
                "install".to_owned(),
                "--channel".to_owned(),
                "stable".to_owned(),
            ],
            true,
        )
        .expect("edit custom tool");
        let (manifest, _, _) = load_manifest(None, &state).expect("reload edited manifest");
        assert_eq!(
            manifest.tools["claude"].args,
            ["install", "--channel", "stable"]
        );

        edit_custom_tool(
            state.clone(),
            "claude",
            "claude-code".to_owned(),
            vec!["claude".to_owned(), "update".to_owned()],
        )
        .expect("rename custom tool");
        let custom =
            UserConfig::load(&state.custom_config_path()).expect("load renamed custom tool");
        assert!(!custom.tools.contains_key("claude"));
        assert_eq!(custom.tools["claude-code"].update, ["claude", "update"]);
        assert_eq!(custom.tools["claude-code"].wait_for, None);

        assert!(
            edit_custom_tool(
                state.clone(),
                "claude-code",
                "brew".to_owned(),
                vec!["brew".to_owned(), "upgrade".to_owned()],
            )
            .is_err()
        );
        let custom =
            UserConfig::load(&state.custom_config_path()).expect("load after rejected rename");
        assert!(custom.tools.contains_key("claude-code"));
        assert!(!custom.tools.contains_key("brew"));

        remove_custom_tool(state.clone(), "claude-code").expect("remove custom tool");
        assert!(!state.custom_config_path().exists());
    }

    #[test]
    fn explicit_user_manifest_layers_on_top_of_builtins() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let path = temporary.path().join("tools.toml");
        let mut user = UserConfig::empty();
        user.tools.insert(
            "example".to_owned(),
            UserTool::custom("example", "example".to_owned(), vec!["update".to_owned()]),
        );
        user.save(&path).expect("save explicit user manifest");

        let (manifest, _, source) =
            load_manifest(Some(path), &state).expect("load layered explicit manifest");

        assert!(matches!(source, ManifestSource::Explicit));
        assert!(manifest.tools.contains_key("dvup"));
        assert!(manifest.tools.contains_key("rustup"));
        assert!(!manifest.tools.contains_key("codex"));
        assert_eq!(manifest.tools["example"].args, ["update"]);
    }

    #[test]
    fn init_writes_only_the_clean_user_layer_template() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let path = temporary.path().join("tools.toml");

        init(&state, Some(path.clone()), false).expect("initialize user manifest");

        let contents = fs::read_to_string(path).expect("read initialized manifest");
        assert_eq!(contents, config::USER_TEMPLATE);
        assert!(!contents.contains("[tools.dvup]"));
        assert!(!contents.contains("program ="));
        UserConfig::parse(&contents).expect("valid initialized user manifest");
    }
}
