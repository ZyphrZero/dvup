use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const STARTER_TEMPLATE: &str = include_str!("../configs/dvup.example.toml");
pub const USER_TEMPLATE: &str = include_str!("../configs/dvup.user.example.toml");

#[cfg(unix)]
const BUN_DEFAULT_PRESET: &str = r#"[tools.bun]
program = "bun"
args = ["upgrade"]
latest = { provider = "github_release", repository = "oven-sh/bun" }
resource_group = "bun-global"
background = "auto"

[[tools.bun.processes]]
name = "bun"
action = "wait""#;

#[cfg(unix)]
const BUN_PLATFORM_PRESET: &str = r#"[tools.bun]
program = "bash"
args = ["-c", "curl -fsSL https://bun.sh/install | bash"]
latest = { provider = "github_release", repository = "oven-sh/bun" }
resource_group = "bun-global"
background = "auto"

[[tools.bun.processes]]
name = "bun"
action = "wait""#;

#[cfg(unix)]
const UV_DEFAULT_PRESET: &str = r#"[tools.uv]
program = "uv"
args = ["self", "update"]
latest = { provider = "github_release", repository = "astral-sh/uv" }
update_version = ["uv", "self", "update", "{version}"]
resource_group = "uv"
background = "auto"

[[tools.uv.processes]]
name = "uv"
action = "wait""#;

#[cfg(unix)]
const UV_PLATFORM_PRESET: &str = r#"[tools.uv]
program = "sh"
args = ["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"]
latest = { provider = "github_release", repository = "astral-sh/uv" }
update_version = ["uv", "self", "update", "{version}"]
platforms = ["macos", "linux"]
resource_group = "uv"
background = "auto"

[[tools.uv.processes]]
name = "uv"
action = "wait""#;

/// Returns the starter template with platform-appropriate built-in commands.
pub fn starter_template() -> String {
    #[cfg(unix)]
    {
        let template = STARTER_TEMPLATE.replacen(BUN_DEFAULT_PRESET, BUN_PLATFORM_PRESET, 1);
        debug_assert_ne!(template, STARTER_TEMPLATE);
        let platform_template = template.replacen(UV_DEFAULT_PRESET, UV_PLATFORM_PRESET, 1);
        debug_assert_ne!(platform_template, template);
        platform_template
    }
    #[cfg(not(unix))]
    {
        STARTER_TEMPLATE.to_owned()
    }
}

/// Top-level dvup manifest.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub tools: BTreeMap<String, Tool>,
    #[serde(default, skip_serializing_if = "GithubMonitorConfig::is_empty")]
    pub(crate) github: GithubMonitorConfig,
}

/// A user-authored manifest containing concise update and probe commands.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    #[serde(default)]
    pub tools: BTreeMap<String, UserTool>,
    #[serde(default, skip_serializing_if = "GithubMonitorConfig::is_empty")]
    pub(crate) github: GithubMonitorConfig,
}

/// GitHub Release repositories managed from the user-authored manifest.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubMonitorConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) monitors: Vec<GithubReleaseMonitor>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReleaseAssetFormat {
    File,
    Zip,
    TarGz,
    Dmg,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReleaseUpdatePolicy {
    #[default]
    Manual,
    Automatic,
}

fn is_manual_release_update_policy(value: &ReleaseUpdatePolicy) -> bool {
    *value == ReleaseUpdatePolicy::Manual
}

fn enabled_by_default() -> bool {
    true
}

fn is_enabled(value: &bool) -> bool {
    *value
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubReleaseMonitor {
    pub(crate) name: String,
    pub(crate) repository: String,
    pub(crate) asset_regex: String,
    pub(crate) target_directory: PathBuf,
    pub(crate) format: ReleaseAssetFormat,
    #[serde(default, skip_serializing_if = "is_manual_release_update_policy")]
    pub(crate) update_policy: ReleaseUpdatePolicy,
    #[serde(default = "enabled_by_default", skip_serializing_if = "is_enabled")]
    pub(crate) cleanup_installer: bool,
    pub(crate) max_download_bytes: u64,
    pub(crate) max_extracted_bytes: u64,
    pub(crate) max_extracted_files: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) strip_components: usize,
    pub(crate) enabled: bool,
}

impl GithubReleaseMonitor {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || self.name.trim() != self.name
            || !self
                .name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
        {
            return Err(Error::InvalidConfig(format!(
                "invalid GitHub release monitor name `{}`",
                self.name
            )));
        }
        let repository_parts = self.repository.split('/').collect::<Vec<_>>();
        if repository_parts.len() != 2
            || repository_parts.iter().any(|part| {
                part.is_empty()
                    || !part.chars().all(|character| {
                        character.is_ascii_alphanumeric() || "-_.".contains(character)
                    })
            })
        {
            return Err(Error::InvalidConfig(format!(
                "GitHub release monitor `{}` repository must be owner/repo",
                self.name
            )));
        }
        if self.asset_regex.is_empty() || self.asset_regex.trim() != self.asset_regex {
            return Err(Error::InvalidConfig(format!(
                "GitHub release monitor `{}` asset_regex cannot be empty or contain surrounding whitespace",
                self.name
            )));
        }
        Regex::new(&self.asset_regex).map_err(|error| {
            Error::InvalidConfig(format!(
                "GitHub release monitor `{}` has invalid asset_regex: {error}",
                self.name
            ))
        })?;
        if !self.target_directory.is_absolute() {
            return Err(Error::InvalidConfig(format!(
                "GitHub release monitor `{}` target_directory must be an absolute path",
                self.name
            )));
        }
        if self.format == ReleaseAssetFormat::File && self.strip_components != 0 {
            return Err(Error::InvalidConfig(format!(
                "GitHub release monitor `{}` cannot strip components from a plain file",
                self.name
            )));
        }
        if self.format == ReleaseAssetFormat::Dmg {
            if self.strip_components != 0 {
                return Err(Error::InvalidConfig(format!(
                    "GitHub release monitor `{}` cannot strip components from a DMG",
                    self.name
                )));
            }
            #[cfg(not(target_os = "macos"))]
            return Err(Error::InvalidConfig(format!(
                "GitHub release monitor `{}` uses DMG installation outside macOS",
                self.name
            )));
            #[cfg(target_os = "macos")]
            if self
                .target_directory
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("app")
            {
                return Err(Error::InvalidConfig(format!(
                    "GitHub release monitor `{}` DMG target must be an absolute .app path",
                    self.name
                )));
            }
        }
        if self.max_download_bytes == 0 || self.max_download_bytes > 8 * 1024 * 1024 * 1024 {
            return Err(Error::InvalidConfig(format!(
                "GitHub release monitor `{}` max_download_bytes must be between 1 and 8589934592",
                self.name
            )));
        }
        match self.format {
            ReleaseAssetFormat::File
                if self.max_extracted_bytes != 0 || self.max_extracted_files != 0 =>
            {
                return Err(Error::InvalidConfig(format!(
                    "GitHub release monitor `{}` plain files cannot set extraction limits",
                    self.name
                )));
            }
            ReleaseAssetFormat::Zip | ReleaseAssetFormat::TarGz | ReleaseAssetFormat::Dmg
                if self.max_extracted_bytes == 0
                    || self.max_extracted_bytes > 16 * 1024 * 1024 * 1024
                    || self.max_extracted_files == 0
                    || self.max_extracted_files > 100_000 =>
            {
                return Err(Error::InvalidConfig(format!(
                    "GitHub release monitor `{}` archive limits must allow 1..=17179869184 bytes and 1..=100000 files",
                    self.name
                )));
            }
            _ => {}
        }
        if self.strip_components > 16 {
            return Err(Error::InvalidConfig(format!(
                "GitHub release monitor `{}` strip_components cannot exceed 16",
                self.name
            )));
        }
        Ok(())
    }
}

impl GithubMonitorConfig {
    pub(crate) fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    fn validate(&self) -> Result<()> {
        let mut names = HashSet::new();
        for monitor in &self.monitors {
            monitor.validate()?;
            if !names.insert(monitor.name.to_ascii_lowercase()) {
                return Err(Error::InvalidConfig(format!(
                    "duplicate GitHub release monitor name `{}`",
                    monitor.name
                )));
            }
        }
        Ok(())
    }
}

const fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// One user-authored tool definition.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserTool {
    pub update: Vec<String>,
    pub probe: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<LatestVersionSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_version: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_auto_background")]
    pub background: ToolBackground,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcessRule>,
    #[serde(
        default = "default_lock_timeout_secs",
        skip_serializing_if = "is_default_lock_timeout_secs"
    )]
    pub lock_timeout_secs: u64,
    #[serde(
        default = "default_retries",
        skip_serializing_if = "is_default_retries"
    )]
    pub retries: u32,
    #[serde(
        default = "default_retry_delay_secs",
        skip_serializing_if = "is_default_retry_delay_secs"
    )]
    pub retry_delay_secs: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_group: Option<String>,
}

/// One tool update definition.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub probe: ToolProbe,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<LatestVersionSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_version: Option<Vec<String>>,
    pub background: ToolBackground,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcessRule>,
    #[serde(default = "default_lock_timeout_secs")]
    pub lock_timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default = "default_retry_delay_secs")]
    pub retry_delay_secs: u64,
    /// Empty means all supported operating systems.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// Tools sharing a resource group are serialized; omitted means the tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_group: Option<String>,
}

/// An explicit command used to detect the target and read its version.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProbe {
    pub program: String,
    pub args: Vec<String>,
}

/// An authoritative registry or repository used to query the latest release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum LatestVersionSource {
    Npm { package: String },
    Pypi { package: String },
    CratesIo { package: String },
    GithubRelease { repository: String },
    GithubTag { repository: String },
}

impl LatestVersionSource {
    fn validate(&self, context: &str) -> Result<()> {
        let value = match self {
            Self::Npm { package } | Self::Pypi { package } | Self::CratesIo { package } => package,
            Self::GithubRelease { repository } | Self::GithubTag { repository } => repository,
        };
        if value.trim().is_empty() {
            return Err(Error::InvalidConfig(format!(
                "{context} has an empty latest-version source"
            )));
        }
        if matches!(self, Self::GithubRelease { .. } | Self::GithubTag { .. }) {
            let mut parts = value.split('/');
            if parts.next().is_none_or(str::is_empty)
                || parts.next().is_none_or(str::is_empty)
                || parts.next().is_some()
            {
                return Err(Error::InvalidConfig(format!(
                    "{context} GitHub repository must use owner/name"
                )));
            }
        }
        Ok(())
    }
}

/// Whether a configured update must leave the invoking executable first.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolBackground {
    #[default]
    Auto,
    Always,
}

/// What the background worker should do when a process rule matches.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessAction {
    #[default]
    Wait,
    Terminate,
    Fail,
}

impl std::fmt::Display for ProcessAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Wait => "wait",
            Self::Terminate => "terminate",
            Self::Fail => "fail",
        };
        formatter.write_str(value)
    }
}

impl std::str::FromStr for ProcessAction {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_lowercase().as_str() {
            "wait" => Ok(Self::Wait),
            "terminate" => Ok(Self::Terminate),
            "fail" => Ok(Self::Fail),
            _ => Err(format!(
                "unknown process action `{value}`; expected wait, terminate, or fail"
            )),
        }
    }
}

/// A process name, optional command-line filter, and handling action.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRule {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_contains: Option<String>,
    #[serde(default)]
    pub action: ProcessAction,
    #[serde(default = "default_terminate_grace_secs")]
    pub terminate_grace_secs: u64,
}

impl ProcessRule {
    pub fn wait(name: String) -> Self {
        Self {
            name,
            command_contains: None,
            action: ProcessAction::Wait,
            terminate_grace_secs: default_terminate_grace_secs(),
        }
    }

    pub fn validate(&self, context: &str) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::InvalidConfig(format!(
                "{context} contains a process rule with an empty name"
            )));
        }
        if self
            .command_contains
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(Error::InvalidConfig(format!(
                "{context} contains an empty command_contains filter"
            )));
        }
        if self.action == ProcessAction::Terminate
            && normalize_process_name(&self.name) == "node"
            && self.command_contains.is_none()
        {
            return Err(Error::InvalidConfig(format!(
                "{context} cannot terminate every Node process; add command_contains to scope the rule"
            )));
        }
        Ok(())
    }
}

impl std::str::FromStr for ProcessRule {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let mut parts = value.splitn(3, ':');
        let action = parts
            .next()
            .ok_or_else(|| "process rule is missing an action".to_owned())?
            .parse()?;
        let name = parts
            .next()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| "process rule must use ACTION:NAME[:COMMAND_CONTAINS]".to_owned())?
            .to_owned();
        let command_contains = parts.next().map(str::to_owned);
        let rule = Self {
            name,
            command_contains,
            action,
            terminate_grace_secs: default_terminate_grace_secs(),
        };
        rule.validate("process rule")
            .map_err(|error| error.to_string())?;
        Ok(rule)
    }
}

impl Tool {
    /// Creates a custom tool with safe updater defaults.
    #[cfg(test)]
    pub fn custom(name: &str, program: String, args: Vec<String>) -> Self {
        let resource_group = inferred_resource_group(&program).map(str::to_owned);
        let platforms = inferred_platforms(&program)
            .iter()
            .map(|platform| (*platform).to_owned())
            .collect();
        Self {
            program,
            args,
            probe: ToolProbe {
                program: name.to_owned(),
                args: vec!["--version".to_owned()],
            },
            latest: None,
            update_version: None,
            background: ToolBackground::Auto,
            processes: vec![ProcessRule::wait(name.to_owned())],
            lock_timeout_secs: default_lock_timeout_secs(),
            retries: default_retries(),
            retry_delay_secs: default_retry_delay_secs(),
            platforms,
            resource_group,
        }
    }

    /// Builds the explicitly configured update command for one target version.
    pub fn update_for_version(&self, name: &str, version: &str) -> Result<(String, Vec<String>)> {
        validate_target_version(version)?;
        let template = self.update_version.as_ref().ok_or_else(|| {
            Error::InvalidConfig(format!(
                "tool `{name}` does not support selecting a target version"
            ))
        })?;
        let command = template
            .iter()
            .map(|part| part.replace("{version}", version))
            .collect();
        split_user_command(name, "update_version", command)
    }

    /// Returns whether this tool is enabled on the current operating system.
    pub fn supports_current_platform(&self) -> bool {
        self.supports_platform(std::env::consts::OS)
    }

    fn supports_platform(&self, platform: &str) -> bool {
        self.platforms.is_empty()
            || self
                .platforms
                .iter()
                .any(|configured| configured.eq_ignore_ascii_case(platform))
    }
}

fn normalized_executable_name(program: &str) -> String {
    let executable = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    [".exe", ".cmd", ".ps1", ".bat"]
        .into_iter()
        .find_map(|suffix| executable.strip_suffix(suffix))
        .unwrap_or(&executable)
        .to_owned()
}

fn inferred_resource_group(program: &str) -> Option<&'static str> {
    match normalized_executable_name(program).as_str() {
        "npm" | "pnpm" => Some("node-global"),
        "bun" => Some("bun-global"),
        "brew" => Some("homebrew"),
        "scoop" => Some("scoop"),
        _ => None,
    }
}

fn inferred_platforms(program: &str) -> &'static [&'static str] {
    match normalized_executable_name(program).as_str() {
        "brew" => &["macos", "linux"],
        "scoop" => &["windows"],
        _ => &[],
    }
}

impl UserTool {
    /// Creates a concise user definition with an explicit version probe.
    pub fn custom(name: &str, program: String, args: Vec<String>) -> Self {
        let mut update = Vec::with_capacity(args.len() + 1);
        update.push(program.clone());
        update.extend(args);
        Self {
            update,
            probe: vec![name.to_owned(), "--version".to_owned()],
            latest: None,
            update_version: None,
            background: ToolBackground::Auto,
            wait_for: None,
            processes: Vec::new(),
            lock_timeout_secs: default_lock_timeout_secs(),
            retries: default_retries(),
            retry_delay_secs: default_retry_delay_secs(),
            platforms: inferred_platforms(&program)
                .iter()
                .map(|platform| (*platform).to_owned())
                .collect(),
            resource_group: inferred_resource_group(&program).map(str::to_owned),
        }
    }

    /// Converts a resolved tool back into the concise user representation.
    pub fn from_tool(name: &str, tool: &Tool) -> Self {
        let mut update = Vec::with_capacity(tool.args.len() + 1);
        update.push(tool.program.clone());
        update.extend(tool.args.clone());
        let mut probe = Vec::with_capacity(tool.probe.args.len() + 1);
        probe.push(tool.probe.program.clone());
        probe.extend(tool.probe.args.clone());
        let default_wait = [name.to_owned()];
        let mut wait_for = Vec::new();
        let mut processes = Vec::new();
        for rule in &tool.processes {
            if rule.action == ProcessAction::Wait
                && rule.command_contains.is_none()
                && rule.terminate_grace_secs == default_terminate_grace_secs()
            {
                wait_for.push(rule.name.clone());
            } else {
                processes.push(rule.clone());
            }
        }
        Self {
            update,
            probe,
            latest: tool.latest.clone(),
            update_version: tool.update_version.clone(),
            background: tool.background,
            wait_for: (wait_for.as_slice() != default_wait).then_some(wait_for),
            processes,
            lock_timeout_secs: tool.lock_timeout_secs,
            retries: tool.retries,
            retry_delay_secs: tool.retry_delay_secs,
            platforms: tool.platforms.clone(),
            resource_group: tool.resource_group.clone(),
        }
    }

    fn resolve(self, name: &str) -> Result<Tool> {
        let (program, args) = split_user_command(name, "update", self.update)?;
        let (probe_program, probe_args) = split_user_command(name, "probe", self.probe)?;
        let mut processes = self.processes;
        processes.extend(
            self.wait_for
                .unwrap_or_else(|| vec![name.to_owned()])
                .into_iter()
                .map(ProcessRule::wait),
        );
        Ok(Tool {
            program,
            args,
            probe: ToolProbe {
                program: probe_program,
                args: probe_args,
            },
            latest: self.latest,
            update_version: self.update_version,
            background: self.background,
            processes,
            lock_timeout_secs: self.lock_timeout_secs,
            retries: self.retries,
            retry_delay_secs: self.retry_delay_secs,
            platforms: self.platforms,
            resource_group: self.resource_group,
        })
    }
}

impl UserConfig {
    /// Returns an empty user manifest ready to receive custom tools.
    pub fn empty() -> Self {
        Self {
            tools: BTreeMap::new(),
            github: GithubMonitorConfig::default(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.github.is_empty()
    }

    /// Reads and validates a user-authored manifest.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err(Error::ConfigNotFound(path.to_path_buf()));
        }
        Self::parse(&fs::read_to_string(path)?)
    }

    /// Parses and validates a user-authored manifest.
    pub fn parse(contents: &str) -> Result<Self> {
        let config: Self = toml::from_str(contents)?;
        config.clone().resolve()?;
        Ok(config)
    }

    /// Resolves concise user definitions into the complete runtime model.
    pub fn resolve(self) -> Result<Config> {
        let mut tools = BTreeMap::new();
        for (name, user_tool) in self.tools {
            if name.trim().is_empty() {
                return Err(Error::InvalidConfig(
                    "tool names cannot be empty".to_owned(),
                ));
            }
            tools.insert(name.clone(), user_tool.resolve(&name)?);
        }
        let config = Config {
            tools,
            github: self.github,
        };
        config.validate_inner(false)?;
        Ok(config)
    }

    /// Validates and atomically saves a user-authored manifest.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.clone().resolve()?;
        write_atomic(path, toml::to_string(self)?.as_bytes())
    }

    /// Validates and atomically saves TOML while preserving editor formatting.
    pub fn save_text(path: &Path, contents: &str) -> Result<()> {
        Self::parse(contents)?;
        write_atomic(path, contents.as_bytes())
    }
}

impl Config {
    /// Parses and validates the complete starter manifest embedded in the binary.
    pub fn starter() -> Self {
        let config: Self = toml::from_str(&starter_template())
            .expect("bundled configs/dvup.example.toml must be valid TOML");
        config
            .validate()
            .expect("bundled configs/dvup.example.toml must be a valid dvup manifest");
        config
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_inner(true)
    }

    fn validate_inner(&self, require_tools: bool) -> Result<()> {
        if require_tools && self.tools.is_empty() {
            return Err(Error::InvalidConfig(
                "at least one [tools.<name>] entry is required".to_owned(),
            ));
        }
        for (name, tool) in &self.tools {
            if name.trim().is_empty() {
                return Err(Error::InvalidConfig(
                    "tool names cannot be empty".to_owned(),
                ));
            }
            if tool.program.trim().is_empty() {
                return Err(Error::InvalidConfig(format!(
                    "tool `{name}` has an empty program"
                )));
            }
            if tool.probe.program.trim().is_empty() {
                return Err(Error::InvalidConfig(format!(
                    "tool `{name}` has an empty probe program"
                )));
            }
            if let Some(latest) = &tool.latest {
                latest.validate(&format!("tool `{name}`"))?;
            }
            validate_update_version_template(name, tool.update_version.as_deref())?;
            if tool.lock_timeout_secs == 0 {
                return Err(Error::InvalidConfig(format!(
                    "tool `{name}` must have lock_timeout_secs greater than zero"
                )));
            }
            for platform in &tool.platforms {
                if !matches!(
                    platform.to_lowercase().as_str(),
                    "windows" | "macos" | "linux"
                ) {
                    return Err(Error::InvalidConfig(format!(
                        "tool `{name}` has unsupported platform `{platform}`; expected windows, macos, or linux"
                    )));
                }
            }
            if let Some(group) = &tool.resource_group {
                if group.is_empty()
                    || !group.chars().all(|character| {
                        character.is_ascii_alphanumeric() || "-_.".contains(character)
                    })
                {
                    return Err(Error::InvalidConfig(format!(
                        "tool `{name}` has invalid resource_group `{group}`; use letters, digits, dash, underscore, or dot"
                    )));
                }
            }
            for rule in &tool.processes {
                rule.validate(&format!("tool `{name}`"))?;
            }
        }
        self.github.validate()?;
        Ok(())
    }
}

fn split_user_command(
    name: &str,
    field: &str,
    command: Vec<String>,
) -> Result<(String, Vec<String>)> {
    let Some((program, args)) = command.split_first() else {
        return Err(Error::InvalidConfig(format!(
            "tool `{name}` has an empty {field} command"
        )));
    };
    if program.trim().is_empty() {
        return Err(Error::InvalidConfig(format!(
            "tool `{name}` has an empty {field} program"
        )));
    }
    Ok((program.clone(), args.to_vec()))
}

fn validate_update_version_template(name: &str, template: Option<&[String]>) -> Result<()> {
    let Some(template) = template else {
        return Ok(());
    };
    split_user_command(name, "update_version", template.to_vec())?;
    let placeholders = template
        .iter()
        .map(|part| part.matches("{version}").count())
        .sum::<usize>();
    if placeholders != 1 {
        return Err(Error::InvalidConfig(format!(
            "tool `{name}` update_version must contain exactly one {{version}} placeholder"
        )));
    }
    Ok(())
}

fn validate_target_version(version: &str) -> Result<()> {
    if version.is_empty()
        || !version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | '_')
        })
    {
        return Err(Error::InvalidConfig(format!(
            "invalid target version `{version}`; use letters, digits, dot, dash, plus, or underscore"
        )));
    }
    Ok(())
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, contents)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn normalize_process_name(name: &str) -> String {
    let normalized = name.trim().to_lowercase();
    normalized
        .strip_suffix(".exe")
        .unwrap_or(&normalized)
        .to_owned()
}

const fn default_lock_timeout_secs() -> u64 {
    86_400
}

fn is_default_lock_timeout_secs(value: &u64) -> bool {
    *value == default_lock_timeout_secs()
}

const fn default_retries() -> u32 {
    8
}

fn is_default_retries(value: &u32) -> bool {
    *value == default_retries()
}

const fn default_retry_delay_secs() -> u64 {
    2
}

fn is_default_retry_delay_secs(value: &u64) -> bool {
    *value == default_retry_delay_secs()
}

fn is_auto_background(value: &ToolBackground) -> bool {
    *value == ToolBackground::Auto
}

const fn default_terminate_grace_secs() -> u64 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_round_trips() {
        let encoded = toml::to_string_pretty(&Config::starter()).expect("serialize starter");
        let decoded: Config = toml::from_str(&encoded).expect("parse starter");

        let document: toml::Value = toml::from_str(&encoded).expect("parse serialized starter");
        assert!(document.get("version").is_none());
        assert_eq!(decoded.tools.len(), 9);
        #[cfg(windows)]
        {
            assert_eq!(decoded.tools["bun"].program, "bun");
            assert_eq!(decoded.tools["bun"].args, ["upgrade"]);
            assert_eq!(decoded.tools["uv"].program, "uv");
            assert_eq!(decoded.tools["uv"].args, ["self", "update"]);
            assert!(decoded.tools["uv"].platforms.is_empty());
        }
        #[cfg(unix)]
        {
            assert_eq!(decoded.tools["bun"].program, "bash");
            assert!(
                decoded.tools["bun"]
                    .args
                    .join(" ")
                    .contains("bun.sh/install")
            );
            assert!(!decoded.tools["bun"].args.iter().any(|arg| arg == "upgrade"));
            assert_eq!(decoded.tools["uv"].program, "sh");
            assert_eq!(decoded.tools["uv"].platforms, ["macos", "linux"]);
            assert!(
                decoded.tools["uv"]
                    .args
                    .join(" ")
                    .contains("astral.sh/uv/install.sh")
            );
        }
        #[cfg(not(any(windows, unix)))]
        assert_eq!(decoded.tools["bun"].args, ["upgrade"]);
        assert_eq!(decoded.tools["rustup"].args, ["update"]);
        assert_eq!(decoded.tools["brew"].args, ["update"]);
        assert_eq!(decoded.tools["brew"].platforms, ["macos", "linux"]);
        assert_eq!(
            decoded.tools["brew"].resource_group.as_deref(),
            Some("homebrew")
        );
        assert_eq!(decoded.tools["scoop"].args, ["update"]);
        assert_eq!(decoded.tools["scoop"].platforms, ["windows"]);
        assert_eq!(
            decoded.tools["bun"].resource_group.as_deref(),
            Some("bun-global")
        );
        assert_eq!(
            decoded.tools["scoop"].resource_group.as_deref(),
            Some("scoop")
        );
        assert!(!decoded.tools.contains_key("npm"));
        assert!(!decoded.tools.contains_key("pnpm"));
        assert!(decoded.tools.contains_key("brew"));
        assert!(!STARTER_TEMPLATE.contains("npm@latest"));
        assert!(!STARTER_TEMPLATE.contains("pnpm self-update"));
        assert_eq!(decoded.tools["dvup"].background, ToolBackground::Always);
        assert_eq!(decoded.tools["dvup"].probe.program, "dvup");
        assert_eq!(decoded.tools["dvup"].program, "cargo");
        assert_eq!(decoded.tools["dvup"].args, ["install", "dvup", "--locked"]);
        for name in ["deno", "mise", "pixi", "uv"] {
            assert!(decoded.tools.contains_key(name));
        }
        for name in ["codex", "claude", "opencode", "hermes"] {
            assert!(!decoded.tools.contains_key(name));
        }
        assert_eq!(decoded.tools["deno"].args, ["upgrade"]);
        assert_eq!(decoded.tools["mise"].args, ["self-update"]);
        assert_eq!(decoded.tools["pixi"].args, ["self-update"]);
        assert!(
            decoded
                .tools
                .values()
                .all(|tool| !tool.probe.program.is_empty())
        );
        assert!(!STARTER_TEMPLATE.contains("[tools.scoop-zedg]"));
        assert!(!STARTER_TEMPLATE.contains("[tools.example]"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let input = r#"
[tools.codex]
program = "npm"
unexpected = true
"#;

        let error = toml::from_str::<Config>(input).expect_err("unknown field should fail");
        assert!(error.to_string().contains("unexpected"));
    }

    #[test]
    fn rejects_obsolete_manifest_version_field() {
        let input = "version = 1\n";

        let runtime_error = toml::from_str::<Config>(input)
            .expect_err("runtime manifest version should be unknown");
        assert!(runtime_error.to_string().contains("version"));

        let user_error =
            UserConfig::parse(input).expect_err("user manifest version should be unknown");
        assert!(user_error.to_string().contains("version"));
    }

    #[test]
    fn rejects_unscoped_node_termination() {
        let input = r#"
[tools.codex]
program = "npm"
background = "auto"

[tools.codex.probe]
program = "codex"
args = ["--version"]

[[tools.codex.processes]]
name = "node"
action = "terminate"
"#;

        let config: Config = toml::from_str(input).expect("parse config");
        let error = config.validate().expect_err("broad Node kill should fail");
        assert!(
            error
                .to_string()
                .contains("cannot terminate every Node process")
        );
    }

    #[test]
    fn parses_scoped_cli_process_rule() {
        let rule: ProcessRule = "terminate:node:@openai/codex"
            .parse()
            .expect("parse process rule");

        assert_eq!(rule.action, ProcessAction::Terminate);
        assert_eq!(rule.name, "node");
        assert_eq!(rule.command_contains.as_deref(), Some("@openai/codex"));
    }

    #[test]
    fn rejects_unscoped_node_cli_rule() {
        let error = "terminate:node"
            .parse::<ProcessRule>()
            .expect_err("broad Node kill should fail");

        assert!(error.contains("cannot terminate every Node process"));
    }

    #[test]
    fn applies_platform_constraints() {
        let scoop = Config::starter().tools.remove("scoop").expect("scoop tool");

        assert!(scoop.supports_platform("windows"));
        assert!(!scoop.supports_platform("linux"));
        assert!(!scoop.supports_platform("macos"));
    }

    #[test]
    fn rejects_unknown_platform() {
        let input = r#"
[tools.example]
program = "example"
platforms = ["freebsd"]
background = "auto"

[tools.example.probe]
program = "example"
args = ["--version"]
"#;

        let config: Config = toml::from_str(input).expect("parse config");
        let error = config.validate().expect_err("unknown platform should fail");
        assert!(error.to_string().contains("unsupported platform `freebsd`"));
    }

    #[test]
    fn rejects_unsafe_resource_group() {
        let input = r#"
[tools.example]
program = "example"
resource_group = "../../outside"
background = "auto"

[tools.example.probe]
program = "example"
args = ["--version"]
"#;

        let config: Config = toml::from_str(input).expect("parse config");
        let error = config
            .validate()
            .expect_err("unsafe resource group should fail");
        assert!(error.to_string().contains("invalid resource_group"));
    }

    #[test]
    fn custom_tool_waits_for_same_named_process() {
        let tool = Tool::custom("claude", "claude".to_owned(), vec!["install".to_owned()]);

        assert_eq!(tool.program, "claude");
        assert_eq!(tool.args, ["install"]);
        assert_eq!(tool.processes.len(), 1);
        assert_eq!(tool.processes[0].name, "claude");
        assert_eq!(tool.processes[0].action, ProcessAction::Wait);
        assert_eq!(tool.resource_group, None);
        assert_eq!(tool.probe.program, "claude");
        assert_eq!(tool.probe.args, ["--version"]);
        assert_eq!(tool.background, ToolBackground::Auto);
    }

    #[test]
    fn user_manifest_requires_update_and_probe_commands() {
        let missing_probe = r#"
[tools.example]
update = ["example", "update"]
"#;
        let missing_update = r#"
[tools.example]
probe = ["example", "--version"]
"#;

        assert!(
            UserConfig::parse(missing_probe)
                .expect_err("probe must be explicit")
                .to_string()
                .contains("missing field `probe`")
        );
        assert!(
            UserConfig::parse(missing_update)
                .expect_err("update must be explicit")
                .to_string()
                .contains("missing field `update`")
        );
    }

    #[test]
    fn user_manifest_is_concise_and_resolves_to_the_runtime_model() {
        let input = r#"[tools.claude]
update = ["claude", "update"]
probe = ["claude", "--version"]
"#;

        let user = UserConfig::parse(input).expect("parse concise user manifest");
        let encoded = toml::to_string(&user).expect("serialize user manifest");
        let runtime = user.resolve().expect("resolve user manifest");
        let claude = &runtime.tools["claude"];

        assert!(encoded.contains("update = [\"claude\", \"update\"]"));
        assert!(encoded.contains("probe = [\"claude\", \"--version\"]"));
        assert!(!encoded.contains("background"));
        assert!(!encoded.contains("lock_timeout_secs"));
        assert_eq!(claude.program, "claude");
        assert_eq!(claude.args, ["update"]);
        assert_eq!(claude.probe.program, "claude");
        assert_eq!(claude.probe.args, ["--version"]);
        assert_eq!(claude.background, ToolBackground::Auto);
        assert_eq!(claude.processes.len(), 1);
        assert_eq!(claude.processes[0].name, "claude");
        assert_eq!(claude.processes[0].action, ProcessAction::Wait);
    }

    #[test]
    fn user_manifest_resolves_latest_source_and_versioned_update_template() {
        let input = r#"[tools.codex]
update = ["npm", "install", "--global", "@openai/codex@latest"]
probe = ["codex", "--version"]
latest = { provider = "npm", package = "@openai/codex" }
update_version = ["npm", "install", "--global", "@openai/codex@{version}"]
"#;

        let runtime = UserConfig::parse(input)
            .expect("parse version-aware user manifest")
            .resolve()
            .expect("resolve version-aware user manifest");
        let codex = &runtime.tools["codex"];

        assert!(matches!(
            &codex.latest,
            Some(LatestVersionSource::Npm { package }) if package == "@openai/codex"
        ));
        assert_eq!(
            codex.update_version.as_deref(),
            Some(
                ["npm", "install", "--global", "@openai/codex@{version}"]
                    .map(str::to_owned)
                    .as_slice()
            )
        );
        assert_eq!(
            codex
                .update_for_version("codex", "0.143.0")
                .expect("render target-version command"),
            (
                "npm".to_owned(),
                ["install", "--global", "@openai/codex@0.143.0"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
        assert!(codex.update_for_version("codex", "1.2.3;remove").is_err());

        let unsupported = Tool::custom("plain", "plain".to_owned(), vec!["update".to_owned()]);
        assert!(unsupported.update_for_version("plain", "1.2.3").is_err());
    }

    #[test]
    fn user_manifest_accepts_an_explicit_pypi_latest_source() {
        let input = r#"[tools.hermes]
update = ["hermes", "update"]
probe = ["hermes", "--version"]
latest = { provider = "pypi", package = "hermes-agent" }
"#;

        let runtime = UserConfig::parse(input)
            .expect("parse PyPI latest source")
            .resolve()
            .expect("resolve PyPI latest source");

        assert!(matches!(
            &runtime.tools["hermes"].latest,
            Some(LatestVersionSource::Pypi { package }) if package == "hermes-agent"
        ));
    }

    #[test]
    fn versioned_update_template_requires_exactly_one_placeholder() {
        for update_version in [
            r#"["npm", "install", "package"]"#,
            r#"["npm", "install", "package@{version}", "{version}"]"#,
        ] {
            let input = format!(
                r#"[tools.example]
update = ["example", "update"]
probe = ["example", "--version"]
update_version = {update_version}
"#
            );
            assert!(UserConfig::parse(&input).is_err(), "input: {input}");
        }
    }

    #[test]
    fn user_manifest_rejects_internal_builtin_fields() {
        let internal_format = r#"[tools.example]
program = "example"
args = ["update"]
background = "auto"

[tools.example.probe]
program = "example"
args = ["--version"]
"#;

        let error = UserConfig::parse(internal_format)
            .expect_err("internal tool fields must not parse as a user manifest");

        let message = error.to_string();
        assert!(message.contains("program") || message.contains("update"));
    }

    #[test]
    fn empty_user_template_is_valid_and_contains_no_builtin_tools() {
        let user = UserConfig::parse(USER_TEMPLATE).expect("valid user template");

        assert!(user.tools.is_empty());
        assert!(
            user.resolve()
                .expect("resolve empty user layer")
                .tools
                .is_empty()
        );
        assert!(!USER_TEMPLATE.contains("[tools.dvup]"));
        for name in ["codex", "claude", "opencode", "hermes"] {
            assert!(USER_TEMPLATE.contains(&format!("# [tools.{name}]")));
            assert!(!STARTER_TEMPLATE.contains(&format!("[tools.{name}]")));
        }
    }

    #[test]
    fn user_manifest_round_trips_github_release_monitors() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let target = toml::Value::String(temporary.path().join("example").display().to_string());
        let input = format!(
            r#"[[github.monitors]]
name = "example"
repository = "owner/repository"
asset_regex = '^example-[0-9]+\.[0-9]+\.[0-9]+-windows-x86_64\.zip$'
target_directory = {target}
format = "zip"
max_download_bytes = 104857600
max_extracted_bytes = 314572800
max_extracted_files = 1000
strip_components = 1
enabled = true
"#
        );

        let user = UserConfig::parse(&input).expect("parse GitHub monitor user manifest");
        let encoded = toml::to_string_pretty(&user).expect("serialize GitHub monitor manifest");
        let decoded = UserConfig::parse(&encoded).expect("reload GitHub monitor manifest");

        assert_eq!(decoded.github.monitors.len(), 1);
        let monitor = &decoded.github.monitors[0];
        assert_eq!(monitor.name, "example");
        assert_eq!(monitor.repository, "owner/repository");
        assert_eq!(
            monitor.asset_regex,
            r"^example-[0-9]+\.[0-9]+\.[0-9]+-windows-x86_64\.zip$"
        );
        assert_eq!(monitor.target_directory, temporary.path().join("example"));
    }

    #[test]
    fn github_release_update_policy_is_manual_by_default_and_can_be_automatic() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let target = toml::Value::String(temporary.path().join("example").display().to_string());
        let input = format!(
            r#"[[github.monitors]]
name = "example"
repository = "owner/repository"
asset_regex = '^example\.zip$'
target_directory = {target}
format = "zip"
max_download_bytes = 1024
max_extracted_bytes = 2048
max_extracted_files = 10
enabled = true
"#
        );

        let manual = UserConfig::parse(&input).expect("manual policy by default");
        assert_eq!(
            manual.github.monitors[0].update_policy,
            ReleaseUpdatePolicy::Manual
        );
        assert!(
            !toml::to_string(&manual)
                .expect("serialize manual policy")
                .contains("update_policy")
        );

        let automatic = UserConfig::parse(&input.replace(
            "enabled = true",
            "update_policy = \"automatic\"\nenabled = true",
        ))
        .expect("explicit automatic policy");
        assert_eq!(
            automatic.github.monitors[0].update_policy,
            ReleaseUpdatePolicy::Automatic
        );
    }

    #[test]
    fn github_installer_cleanup_is_enabled_by_default_and_can_be_disabled() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let target = toml::Value::String(temporary.path().join("example").display().to_string());
        let input = format!(
            r#"[[github.monitors]]
name = "example"
repository = "owner/repository"
asset_regex = '^example\.zip$'
target_directory = {target}
format = "zip"
max_download_bytes = 1024
max_extracted_bytes = 2048
max_extracted_files = 10
enabled = true
"#
        );

        let cleanup = UserConfig::parse(&input).expect("cleanup by default");
        assert!(cleanup.github.monitors[0].cleanup_installer);
        assert!(
            !toml::to_string(&cleanup)
                .expect("serialize cleanup default")
                .contains("cleanup_installer")
        );

        let retained = UserConfig::parse(&input.replace(
            "enabled = true",
            "cleanup_installer = false\nenabled = true",
        ))
        .expect("retain installer");
        assert!(!retained.github.monitors[0].cleanup_installer);
    }

    #[test]
    fn user_manifest_rejects_legacy_github_asset_pattern() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let target = toml::Value::String(temporary.path().join("example").display().to_string());
        let input = format!(
            r#"[[github.monitors]]
name = "example"
repository = "owner/repository"
asset_pattern = '^example\.zip$'
target_directory = {target}
format = "zip"
max_download_bytes = 1024
max_extracted_bytes = 2048
max_extracted_files = 10
enabled = true
"#
        );

        let error = UserConfig::parse(&input)
            .expect_err("legacy asset_pattern must not parse in dvup_custom.toml");
        assert!(error.to_string().contains("asset_pattern"));
    }

    #[test]
    fn github_release_monitors_require_unique_names_valid_regex_and_absolute_targets() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let monitor = GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/repository".to_owned(),
            asset_regex: r"^tool-[0-9.]+-windows\.zip$".to_owned(),
            target_directory: temporary.path().join("tool"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 100 * 1024 * 1024,
            max_extracted_bytes: 300 * 1024 * 1024,
            max_extracted_files: 1_000,
            strip_components: 1,
            enabled: true,
        };
        let mut github = GithubMonitorConfig {
            monitors: vec![monitor.clone()],
        };
        assert!(github.validate().is_ok());

        github.monitors.push(monitor);
        assert!(github.validate().is_err());
        github.monitors[1].name = "second".to_owned();
        github.monitors[1].target_directory = PathBuf::from("relative");
        assert!(github.validate().is_err());

        github.monitors.truncate(1);
        github.monitors[0].asset_regex = "[unterminated".to_owned();
        let error = github.validate().expect_err("invalid regex must fail");
        assert!(error.to_string().contains("invalid asset_regex"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_dmg_monitor_requires_an_app_target_and_archive_limits() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let mut monitor = GithubReleaseMonitor {
            name: "reqable".to_owned(),
            repository: "reqable/reqable-app".to_owned(),
            asset_regex: r"^reqable-app-macos-arm64\.dmg$".to_owned(),
            target_directory: temporary.path().join("Reqable.app"),
            format: ReleaseAssetFormat::Dmg,
            update_policy: ReleaseUpdatePolicy::Automatic,
            cleanup_installer: true,
            max_download_bytes: 100 * 1024 * 1024,
            max_extracted_bytes: 500 * 1024 * 1024,
            max_extracted_files: 20_000,
            strip_components: 0,
            enabled: true,
        };

        assert!(monitor.validate().is_ok());

        monitor.target_directory = temporary.path().join("Reqable");
        assert!(monitor.validate().is_err());
        monitor.target_directory = temporary.path().join("Reqable.app");
        monitor.max_extracted_bytes = 0;
        assert!(monitor.validate().is_err());
        monitor.max_extracted_bytes = 500 * 1024 * 1024;
        monitor.strip_components = 1;
        assert!(monitor.validate().is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn dmg_monitors_are_rejected_outside_macos() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let monitor = GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/repository".to_owned(),
            asset_regex: r"^example\.dmg$".to_owned(),
            target_directory: temporary.path().join("Example.app"),
            format: ReleaseAssetFormat::Dmg,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        };

        assert!(monitor.validate().is_err());
    }

    #[test]
    fn npm_and_pnpm_commands_share_the_node_global_resource() {
        for program in ["npm", "npm.cmd", "pnpm", "C:\\tools\\pnpm.cmd"] {
            let tool = Tool::custom(
                "package",
                program.to_owned(),
                vec![
                    "add".to_owned(),
                    "--global".to_owned(),
                    "package@latest".to_owned(),
                ],
            );

            assert_eq!(tool.resource_group.as_deref(), Some("node-global"));
        }
    }

    #[test]
    fn brew_commands_use_homebrew_defaults_on_macos_and_linux() {
        for program in [
            "brew",
            "/opt/homebrew/bin/brew",
            "/home/linuxbrew/.linuxbrew/bin/brew",
        ] {
            let tool = Tool::custom(
                "ripgrep",
                program.to_owned(),
                vec!["upgrade".to_owned(), "ripgrep".to_owned()],
            );

            assert_eq!(tool.args, ["upgrade", "ripgrep"]);
            assert_eq!(tool.resource_group.as_deref(), Some("homebrew"));
            assert_eq!(tool.platforms, ["macos", "linux"]);
        }
    }

    #[test]
    fn bun_and_scoop_commands_use_their_own_package_manager_resources() {
        let bun = Tool::custom(
            "example-bun-package",
            "/home/user/.bun/bin/bun".to_owned(),
            vec![
                "add".to_owned(),
                "--global".to_owned(),
                "example@latest".to_owned(),
            ],
        );
        let scoop = Tool::custom(
            "scoop-zed",
            "scoop.ps1".to_owned(),
            vec!["update".to_owned(), "zed".to_owned()],
        );

        assert_eq!(bun.resource_group.as_deref(), Some("bun-global"));
        assert!(bun.platforms.is_empty());
        assert_eq!(scoop.resource_group.as_deref(), Some("scoop"));
        assert_eq!(scoop.platforms, ["windows"]);
    }
}
