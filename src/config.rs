use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const STARTER_TEMPLATE: &str = include_str!("../configs/dvup.example.toml");
pub const USER_TEMPLATE: &str = include_str!("../configs/dvup.user.example.toml");

const BUN_DEFAULT_PRESET: &str = r#"[tools.bun]
program = "bun"
args = ["upgrade"]
resource_group = "bun-global"
background = "auto"

[[tools.bun.processes]]
name = "bun"
action = "wait""#;

#[cfg(windows)]
const BUN_PLATFORM_PRESET: &str = r#"[tools.bun]
program = "Invoke-Expression"
args = ["Invoke-RestMethod https://bun.sh/install.ps1 | Invoke-Expression"]
resource_group = "bun-global"
background = "auto"

[[tools.bun.processes]]
name = "bun"
action = "wait""#;

#[cfg(unix)]
const BUN_PLATFORM_PRESET: &str = r#"[tools.bun]
program = "bash"
args = ["-c", "curl -fsSL https://bun.sh/install | bash"]
resource_group = "bun-global"
background = "auto"

[[tools.bun.processes]]
name = "bun"
action = "wait""#;

const UV_DEFAULT_PRESET: &str = r#"[tools.uv]
program = "uv"
args = ["self", "update"]
resource_group = "uv"
background = "auto"

[[tools.uv.processes]]
name = "uv"
action = "wait""#;

#[cfg(windows)]
const UV_PLATFORM_PRESET: &str = r#"[tools.uv]
program = "powershell"
args = ["-ExecutionPolicy", "ByPass", "-c", "irm https://astral.sh/uv/install.ps1 | iex"]
platforms = ["windows"]
resource_group = "uv"
background = "auto"

[[tools.uv.processes]]
name = "uv"
action = "wait""#;

#[cfg(unix)]
const UV_PLATFORM_PRESET: &str = r#"[tools.uv]
program = "sh"
args = ["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"]
platforms = ["macos", "linux"]
resource_group = "uv"
background = "auto"

[[tools.uv.processes]]
name = "uv"
action = "wait""#;

/// Returns the starter template with platform-appropriate built-in commands.
pub fn starter_template() -> String {
    #[cfg(any(windows, unix))]
    {
        let template = STARTER_TEMPLATE.replacen(BUN_DEFAULT_PRESET, BUN_PLATFORM_PRESET, 1);
        debug_assert_ne!(template, STARTER_TEMPLATE);
        let platform_template = template.replacen(UV_DEFAULT_PRESET, UV_PLATFORM_PRESET, 1);
        debug_assert_ne!(platform_template, template);
        platform_template
    }
    #[cfg(not(any(windows, unix)))]
    {
        STARTER_TEMPLATE.to_owned()
    }
}

/// Top-level dvup manifest.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub tools: BTreeMap<String, Tool>,
}

/// A user-authored manifest containing concise update and probe commands.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub version: u32,
    #[serde(default)]
    pub tools: BTreeMap<String, UserTool>,
}

/// One user-authored tool definition.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserTool {
    pub update: Vec<String>,
    pub probe: Vec<String>,
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
            background: ToolBackground::Auto,
            processes: vec![ProcessRule::wait(name.to_owned())],
            lock_timeout_secs: default_lock_timeout_secs(),
            retries: default_retries(),
            retry_delay_secs: default_retry_delay_secs(),
            platforms,
            resource_group,
        }
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
            version: 1,
            tools: BTreeMap::new(),
        }
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
        if self.version != 1 {
            return Err(Error::InvalidConfig(format!(
                "unsupported version {}; expected 1",
                self.version
            )));
        }
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
            version: self.version,
            tools,
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
        if self.version != 1 {
            return Err(Error::InvalidConfig(format!(
                "unsupported version {}; expected 1",
                self.version
            )));
        }
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

        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.tools.len(), 9);
        #[cfg(windows)]
        {
            assert_eq!(decoded.tools["bun"].program, "Invoke-Expression");
            assert!(
                decoded.tools["bun"]
                    .args
                    .join(" ")
                    .contains("bun.sh/install.ps1")
            );
            assert!(!decoded.tools["bun"].args.iter().any(|arg| arg == "upgrade"));
            assert_eq!(decoded.tools["uv"].program, "powershell");
            assert_eq!(decoded.tools["uv"].platforms, ["windows"]);
            assert!(
                decoded.tools["uv"]
                    .args
                    .join(" ")
                    .contains("astral.sh/uv/install.ps1")
            );
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
version = 1

[tools.codex]
program = "npm"
unexpected = true
"#;

        let error = toml::from_str::<Config>(input).expect_err("unknown field should fail");
        assert!(error.to_string().contains("unexpected"));
    }

    #[test]
    fn rejects_unscoped_node_termination() {
        let input = r#"
version = 1

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
version = 1

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
version = 1

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
version = 1

[tools.example]
update = ["example", "update"]
"#;
        let missing_update = r#"
version = 1

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
        let input = r#"version = 1

[tools.claude]
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
    fn user_manifest_rejects_internal_builtin_fields() {
        let internal_format = r#"version = 1

[tools.example]
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
