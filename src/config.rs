use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const DEFAULT_CONFIG_FILE: &str = ".dvup.toml";
pub const LEGACY_CONFIG_FILE: &str = ".kvdev.toml";
pub const STARTER_TEMPLATE: &str = include_str!("../configs/dvup.example.toml");

const BUN_DEFAULT_PRESET: &str = r#"[tools.bun]
program = "bun"
args = ["upgrade"]
lock_processes = ["bun"]
resource_group = "bun-global""#;

#[cfg(windows)]
const BUN_PLATFORM_PRESET: &str = r#"[tools.bun]
program = "Invoke-Expression"
args = ["Invoke-RestMethod https://bun.sh/install.ps1 | Invoke-Expression"]
lock_processes = ["bun"]
resource_group = "bun-global""#;

#[cfg(unix)]
const BUN_PLATFORM_PRESET: &str = r#"[tools.bun]
program = "bash"
args = ["-c", "curl -fsSL https://bun.sh/install | bash"]
lock_processes = ["bun"]
resource_group = "bun-global""#;

const UV_DEFAULT_PRESET: &str = r#"[tools.uv]
program = "uv"
args = ["self", "update"]
lock_processes = ["uv"]"#;

#[cfg(windows)]
const UV_PLATFORM_PRESET: &str = r#"[tools.uv]
program = "powershell"
args = ["-ExecutionPolicy", "ByPass", "-c", "irm https://astral.sh/uv/install.ps1 | iex"]
lock_processes = ["uv"]
platforms = ["windows"]"#;

#[cfg(unix)]
const UV_PLATFORM_PRESET: &str = r#"[tools.uv]
program = "sh"
args = ["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"]
lock_processes = ["uv"]
platforms = ["macos", "linux"]"#;

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

/// One tool update definition.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Legacy shorthand for process rules whose action is `wait`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lock_processes: Vec<String>,
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
    pub fn custom(name: &str, program: String, args: Vec<String>) -> Self {
        let resource_group = inferred_resource_group(&program).map(str::to_owned);
        let platforms = inferred_platforms(&program)
            .iter()
            .map(|platform| (*platform).to_owned())
            .collect();
        Self {
            program,
            args,
            lock_processes: vec![name.to_owned()],
            processes: Vec::new(),
            lock_timeout_secs: default_lock_timeout_secs(),
            retries: default_retries(),
            retry_delay_secs: default_retry_delay_secs(),
            platforms,
            resource_group,
        }
    }

    /// Expands legacy lock names into explicit wait rules.
    pub fn process_rules(&self) -> Vec<ProcessRule> {
        let mut rules = self.processes.clone();
        rules.extend(self.lock_processes.iter().map(|name| ProcessRule {
            name: name.clone(),
            command_contains: None,
            action: ProcessAction::Wait,
            terminate_grace_secs: default_terminate_grace_secs(),
        }));
        rules
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

impl Config {
    /// Returns an empty user manifest ready to receive custom tools.
    pub fn empty() -> Self {
        Self {
            version: 1,
            tools: BTreeMap::new(),
        }
    }

    /// Parses and validates the starter manifest embedded in the binary.
    pub fn starter() -> Self {
        let mut config: Self = toml::from_str(&starter_template())
            .expect("bundled configs/dvup.example.toml must be valid TOML");
        config.apply_platform_defaults();
        config
            .validate()
            .expect("bundled configs/dvup.example.toml must be a valid dvup manifest");
        config
    }

    /// Reads and validates a manifest.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err(Error::ConfigNotFound(path.to_path_buf()));
        }
        let contents = fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&contents)?;
        config.apply_platform_defaults();
        config.validate()?;
        Ok(config)
    }

    fn apply_platform_defaults(&mut self) {
        for tool in self.tools.values_mut() {
            if tool.resource_group.is_none() {
                tool.resource_group = inferred_resource_group(&tool.program).map(str::to_owned);
            }
            if tool.platforms.is_empty() {
                tool.platforms = inferred_platforms(&tool.program)
                    .iter()
                    .map(|platform| (*platform).to_owned())
                    .collect();
            }
        }

        #[cfg(windows)]
        if let Some(bun) = self.tools.get_mut("bun") {
            let legacy_upgrade = bun.program.eq_ignore_ascii_case("bun") && bun.args == ["upgrade"];
            let nested_powershell = bun.program.eq_ignore_ascii_case("powershell")
                || bun.program.eq_ignore_ascii_case("powershell.exe");
            if legacy_upgrade
                || (nested_powershell && bun.args.join(" ").contains("bun.sh/install.ps1"))
            {
                bun.program = "Invoke-Expression".to_owned();
                bun.args = vec![
                    "Invoke-RestMethod https://bun.sh/install.ps1 | Invoke-Expression".to_owned(),
                ];
            }
        }

        #[cfg(windows)]
        if let Some(uv) = self.tools.get_mut("uv") {
            let legacy_self_update =
                uv.program.eq_ignore_ascii_case("uv") && uv.args == ["self", "update"];
            let official_installer = uv.args.join(" ").contains("astral.sh/uv/install.ps1");
            if legacy_self_update {
                uv.program = "powershell".to_owned();
                uv.args = vec![
                    "-ExecutionPolicy".to_owned(),
                    "ByPass".to_owned(),
                    "-c".to_owned(),
                    "irm https://astral.sh/uv/install.ps1 | iex".to_owned(),
                ];
            }
            if (legacy_self_update || official_installer) && uv.platforms.is_empty() {
                uv.platforms = vec!["windows".to_owned()];
            }
        }

        #[cfg(unix)]
        if let Some(bun) = self.tools.get_mut("bun") {
            let legacy_upgrade = bun.program.eq_ignore_ascii_case("bun") && bun.args == ["upgrade"];
            if legacy_upgrade {
                bun.program = "bash".to_owned();
                bun.args = vec![
                    "-c".to_owned(),
                    "curl -fsSL https://bun.sh/install | bash".to_owned(),
                ];
            }
        }

        #[cfg(unix)]
        if let Some(uv) = self.tools.get_mut("uv") {
            let legacy_self_update =
                uv.program.eq_ignore_ascii_case("uv") && uv.args == ["self", "update"];
            let official_installer = uv.args.join(" ").contains("astral.sh/uv/install.sh");
            if legacy_self_update {
                uv.program = "sh".to_owned();
                uv.args = vec![
                    "-c".to_owned(),
                    "curl -LsSf https://astral.sh/uv/install.sh | sh".to_owned(),
                ];
            }
            if (legacy_self_update || official_installer) && uv.platforms.is_empty() {
                uv.platforms = vec!["macos".to_owned(), "linux".to_owned()];
            }
        }
    }

    /// Validates and atomically saves a non-empty manifest.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, toml::to_string_pretty(self)?)?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(temporary, path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::InvalidConfig(format!(
                "unsupported version {}; expected 1",
                self.version
            )));
        }
        if self.tools.is_empty() {
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
            let rules = tool.process_rules();
            for rule in &rules {
                rule.validate(&format!("tool `{name}`"))?;
            }
        }
        Ok(())
    }
}

fn normalize_process_name(name: &str) -> String {
    let normalized = name.trim().to_lowercase();
    normalized
        .strip_suffix(".exe")
        .unwrap_or(&normalized)
        .to_owned()
}

/// Resolves the default manifest in the current directory.
pub fn default_path() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(DEFAULT_CONFIG_FILE))
}

/// Resolves the pre-rename manifest in the current directory.
pub fn legacy_default_path() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(LEGACY_CONFIG_FILE))
}

const fn default_lock_timeout_secs() -> u64 {
    86_400
}

const fn default_retries() -> u32 {
    8
}

const fn default_retry_delay_secs() -> u64 {
    2
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

        let codex = decoded.tools.get("codex").expect("codex tool");
        assert_eq!(decoded.version, 1);
        assert_eq!(codex.program, "npm");
        assert_eq!(codex.args, ["install", "--global", "@openai/codex@latest"]);
        assert_eq!(decoded.tools.len(), 6);
        assert_eq!(codex.processes.len(), 2);
        assert_eq!(codex.processes[1].action, ProcessAction::Terminate);
        assert_eq!(
            codex.processes[1].command_contains.as_deref(),
            Some("@openai/codex")
        );
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
            decoded.tools["codex"].resource_group.as_deref(),
            Some("node-global")
        );
        assert_eq!(
            decoded.tools["scoop"].resource_group.as_deref(),
            Some("scoop")
        );
        assert!(!decoded.tools.contains_key("npm"));
        assert!(!decoded.tools.contains_key("pnpm"));
        assert!(decoded.tools.contains_key("brew"));
        assert!(!STARTER_TEMPLATE.contains("npm@latest"));
        assert!(!STARTER_TEMPLATE.contains("self-update"));
        assert!(STARTER_TEMPLATE.contains("[tools.scoop-zedg]"));
        assert!(STARTER_TEMPLATE.contains("args = [\"update\", \"zedg\"]"));
    }

    #[test]
    fn migrates_legacy_bun_upgrade_config_to_official_installer() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("legacy.toml");
        std::fs::write(
            &path,
            r#"version = 1

[tools.bun]
program = "bun"
args = ["upgrade"]
lock_processes = ["bun"]
"#,
        )
        .expect("write legacy config");

        let config = Config::load(&path).expect("load migrated config");
        let bun = &config.tools["bun"];
        #[cfg(windows)]
        {
            assert_eq!(bun.program, "Invoke-Expression");
            assert!(bun.args.join(" ").contains("bun.sh/install.ps1"));
        }
        #[cfg(unix)]
        {
            assert_eq!(bun.program, "bash");
            assert!(bun.args.join(" ").contains("bun.sh/install"));
        }
        #[cfg(not(any(windows, unix)))]
        assert_eq!(bun.args, ["upgrade"]);
        assert_eq!(bun.resource_group.as_deref(), Some("bun-global"));
    }

    #[test]
    fn migrates_legacy_uv_self_update_to_official_installer() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("legacy-uv.toml");
        std::fs::write(
            &path,
            r#"version = 1

[tools.uv]
program = "uv"
args = ["self", "update"]
lock_processes = ["uv"]
"#,
        )
        .expect("write legacy config");

        let config = Config::load(&path).expect("load migrated config");
        let uv = &config.tools["uv"];
        #[cfg(windows)]
        {
            assert_eq!(uv.program, "powershell");
            assert!(uv.args.join(" ").contains("astral.sh/uv/install.ps1"));
            assert_eq!(uv.platforms, ["windows"]);
        }
        #[cfg(unix)]
        {
            assert_eq!(uv.program, "sh");
            assert!(uv.args.join(" ").contains("astral.sh/uv/install.sh"));
            assert_eq!(uv.platforms, ["macos", "linux"]);
        }
        #[cfg(not(any(windows, unix)))]
        assert_eq!(uv.args, ["self", "update"]);
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
        assert_eq!(tool.lock_processes, ["claude"]);
        assert_eq!(tool.resource_group, None);
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

    #[test]
    fn loading_a_brew_command_applies_safe_defaults_to_older_configs() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("brew.toml");
        std::fs::write(
            &path,
            r#"version = 1

[tools.ripgrep]
program = "brew"
args = ["upgrade", "ripgrep"]
"#,
        )
        .expect("write brew config");

        let config = Config::load(&path).expect("load brew config");
        let ripgrep = &config.tools["ripgrep"];
        assert_eq!(ripgrep.resource_group.as_deref(), Some("homebrew"));
        assert_eq!(ripgrep.platforms, ["macos", "linux"]);
    }
}
