use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

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

/// A strict user-authored manifest containing concise declarations only.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, CommandSpec>,
    #[serde(default, skip_serializing_if = "GithubConfig::is_empty")]
    pub(crate) github: GithubConfig,
}

/// One user-authored command declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandSpec {
    Package(PackageCommandSpec),
    Custom(CustomCommandSpec),
}

/// The package managers whose update behavior is locally deterministic.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PackageManager {
    Homebrew,
    Npm,
    Pnpm,
    Cargo,
    Pipx,
    Uv,
}

impl PackageManager {
    pub(crate) const ALL: [Self; 6] = [
        Self::Homebrew,
        Self::Npm,
        Self::Pnpm,
        Self::Cargo,
        Self::Pipx,
        Self::Uv,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Homebrew => "homebrew",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Cargo => "cargo",
            Self::Pipx => "pipx",
            Self::Uv => "uv",
        }
    }

    pub(crate) fn cycle(self, delta: isize) -> Self {
        let index = Self::ALL
            .iter()
            .position(|manager| *manager == self)
            .expect("package manager belongs to ALL");
        Self::ALL[(index as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize]
    }

    pub(crate) fn update_command(self, package: &str) -> Vec<String> {
        match self {
            Self::Homebrew => command_parts(&["brew", "upgrade", package]),
            Self::Npm => {
                command_parts(&["npm", "install", "--global", &format!("{package}@latest")])
            }
            Self::Pnpm => command_parts(&["pnpm", "add", "--global", &format!("{package}@latest")]),
            Self::Cargo => command_parts(&["cargo", "install", package]),
            Self::Pipx => command_parts(&["pipx", "upgrade", package]),
            Self::Uv => command_parts(&["uv", "tool", "upgrade", package]),
        }
    }

    pub(crate) fn update_version_command(self, package: &str) -> Option<Vec<String>> {
        match self {
            Self::Homebrew => None,
            Self::Npm => Some(command_parts(&[
                "npm",
                "install",
                "--global",
                &format!("{package}@{{version}}"),
            ])),
            Self::Pnpm => Some(command_parts(&[
                "pnpm",
                "add",
                "--global",
                &format!("{package}@{{version}}"),
            ])),
            Self::Cargo => Some(command_parts(&[
                "cargo",
                "install",
                package,
                "--version",
                "{version}",
            ])),
            Self::Pipx => Some(command_parts(&[
                "pipx",
                "install",
                "--force",
                &format!("{package}=={{version}}"),
            ])),
            Self::Uv => Some(command_parts(&[
                "uv",
                "tool",
                "install",
                "--force",
                &format!("{package}=={{version}}"),
            ])),
        }
    }

    pub(crate) fn latest_source(self, package: &str) -> LatestVersionSource {
        match self {
            Self::Homebrew => LatestVersionSource::Homebrew {
                formula: package.to_owned(),
            },
            Self::Npm | Self::Pnpm => LatestVersionSource::Npm {
                package: package.to_owned(),
            },
            Self::Cargo => LatestVersionSource::CratesIo {
                package: package.to_owned(),
            },
            Self::Pipx | Self::Uv => LatestVersionSource::Pypi {
                package: package.to_owned(),
            },
        }
    }

    pub(crate) fn platforms(self) -> Vec<String> {
        match self {
            Self::Homebrew => vec!["macos".to_owned(), "linux".to_owned()],
            Self::Npm | Self::Pnpm | Self::Cargo | Self::Pipx | Self::Uv => Vec::new(),
        }
    }

    pub(crate) const fn resource_group(self) -> &'static str {
        match self {
            Self::Homebrew => "homebrew",
            Self::Npm | Self::Pnpm => "node-global",
            Self::Cargo => "cargo-global",
            Self::Pipx | Self::Uv => "python-global",
        }
    }
}

fn command_parts(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn default_probe_args() -> Vec<String> {
    vec!["--version".to_owned()]
}

fn is_default_probe_args(args: &[String]) -> bool {
    args == ["--version"]
}

/// A package-manager-backed command declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageCommandSpec {
    pub(crate) manager: PackageManager,
    pub(crate) package: String,
    pub(crate) executable: String,
    #[serde(
        default = "default_probe_args",
        skip_serializing_if = "is_default_probe_args"
    )]
    pub(crate) probe_args: Vec<String>,
}

/// A self-updating command declaration with an optional authoritative source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomCommandSpec {
    pub(crate) update: Vec<String>,
    pub(crate) probe: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) latest: Option<LatestVersionSource>,
}

/// GitHub Release declarations keyed by their monitor name.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) monitors: BTreeMap<String, GithubMonitorSpec>,
}

impl GithubConfig {
    fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }
}

/// One concise GitHub Release declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubMonitorSpec {
    pub(crate) repository: String,
    pub(crate) asset: AssetSelector,
    pub(crate) install: GithubInstallSpec,
    #[serde(default, skip_serializing_if = "is_manual_release_update_policy")]
    pub(crate) update_policy: ReleaseUpdatePolicy,
    #[serde(default = "enabled_by_default", skip_serializing_if = "is_enabled")]
    pub(crate) enabled: bool,
}

/// Installation behavior inferred after the user chooses a real release asset.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum GithubInstallSpec {
    UserDirectory,
    MacosApplication { application: String },
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

impl ReleaseAssetFormat {
    pub(crate) fn matches_name(self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        match self {
            Self::File => {
                !name.ends_with(".zip")
                    && !name.ends_with(".tar.gz")
                    && !name.ends_with(".tgz")
                    && !name.ends_with(".dmg")
            }
            Self::Zip => name.ends_with(".zip"),
            Self::TarGz => name.ends_with(".tar.gz") || name.ends_with(".tgz"),
            Self::Dmg => name.ends_with(".dmg"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssetOperatingSystem {
    Macos,
    Linux,
    Windows,
}

impl AssetOperatingSystem {
    pub(crate) fn current() -> Option<Self> {
        match std::env::consts::OS {
            "macos" => Some(Self::Macos),
            "linux" => Some(Self::Linux),
            "windows" => Some(Self::Windows),
            _ => None,
        }
    }

    pub(crate) fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Macos => &["macos", "darwin", "osx", "apple-darwin"],
            Self::Linux => &["linux", "unknown-linux"],
            Self::Windows => &["windows", "win32", "win64", "pc-windows"],
        }
    }

    pub(crate) fn matches_name(self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.aliases()
            .iter()
            .any(|alias| contains_identifier(&name, alias))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssetArchitecture {
    Aarch64,
    X86_64,
}

impl AssetArchitecture {
    pub(crate) fn current() -> Option<Self> {
        match std::env::consts::ARCH {
            "aarch64" => Some(Self::Aarch64),
            "x86_64" => Some(Self::X86_64),
            _ => None,
        }
    }

    pub(crate) fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Aarch64 => &["aarch64", "arm64"],
            Self::X86_64 => &["x86_64", "x86-64", "amd64", "x64"],
        }
    }

    pub(crate) fn matches_name(self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.aliases()
            .iter()
            .any(|alias| contains_identifier(&name, alias))
    }
}

/// A semantic selector derived from one explicitly chosen release asset.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetSelector {
    pub(crate) product: String,
    pub(crate) os: AssetOperatingSystem,
    pub(crate) arch: AssetArchitecture,
    pub(crate) format: ReleaseAssetFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) variant: Option<String>,
}

impl AssetSelector {
    pub(crate) fn from_release_asset(asset_name: &str, release_tag: &str) -> Result<Self> {
        let os = AssetOperatingSystem::current().ok_or_else(|| {
            Error::Message(format!(
                "GitHub Release assets are unsupported on {}",
                std::env::consts::OS
            ))
        })?;
        let arch = AssetArchitecture::current().ok_or_else(|| {
            Error::Message(format!(
                "GitHub Release assets are unsupported on {}",
                std::env::consts::ARCH
            ))
        })?;
        let format = [
            ReleaseAssetFormat::TarGz,
            ReleaseAssetFormat::Zip,
            ReleaseAssetFormat::Dmg,
            ReleaseAssetFormat::File,
        ]
        .into_iter()
        .find(|format| format.matches_name(asset_name))
        .expect("plain file format matches every remaining name");
        let lower = asset_name.to_ascii_lowercase();
        let stem = lower
            .strip_suffix(".tar.gz")
            .or_else(|| lower.strip_suffix(".tgz"))
            .or_else(|| lower.strip_suffix(".zip"))
            .or_else(|| lower.strip_suffix(".dmg"))
            .unwrap_or(&lower);
        let tagged_boundary = release_version_boundary(stem, release_tag);
        let inferred_boundaries = tagged_boundary.is_none().then(|| {
            stem.char_indices().filter_map(|(index, character)| {
                if !character.is_ascii_digit() || index == 0 {
                    return None;
                }
                let previous = stem[..index].chars().next_back()?;
                if "-_.".contains(previous) {
                    return Some(index);
                }
                if matches!(previous, 'v' | 'V') {
                    let version_prefix = index - previous.len_utf8();
                    if version_prefix > 0
                        && stem[..version_prefix]
                            .chars()
                            .next_back()
                            .is_some_and(|separator| "-_.".contains(separator))
                    {
                        return Some(version_prefix);
                    }
                }
                None
            })
        });
        let boundary = tagged_boundary
            .into_iter()
            .chain(inferred_boundaries.into_iter().flatten())
            .chain(
                os.aliases()
                    .iter()
                    .chain(arch.aliases())
                    .filter_map(|alias| {
                        stem.match_indices(alias)
                            .map(|(index, _)| index)
                            .find(|index| {
                                *index > 0
                                    && stem[..*index]
                                        .chars()
                                        .next_back()
                                        .is_some_and(|previous| "-_.".contains(previous))
                            })
                    }),
            )
            .min()
            .unwrap_or(stem.len());
        let product = stem[..boundary]
            .trim_end_matches(|character| "-_.".contains(character))
            .to_owned();
        if product.is_empty() {
            return Err(Error::Message(format!(
                "could not infer a product identifier from release asset `{asset_name}`"
            )));
        }
        let variant = ["musl", "gnu", "portable"]
            .into_iter()
            .find(|variant| contains_identifier(stem, variant))
            .map(str::to_owned);
        let selector = Self {
            product,
            os,
            arch,
            format,
            variant,
        };
        selector.validate("selected release asset")?;
        Ok(selector)
    }

    pub(crate) fn matches(&self, asset_name: &str) -> bool {
        let normalized = asset_name.to_ascii_lowercase();
        contains_identifier(&normalized, &self.product)
            && self
                .os
                .aliases()
                .iter()
                .any(|alias| contains_identifier(&normalized, alias))
            && self
                .arch
                .aliases()
                .iter()
                .any(|alias| contains_identifier(&normalized, alias))
            && self.format.matches_name(&normalized)
            && self
                .variant
                .as_deref()
                .is_none_or(|variant| contains_identifier(&normalized, variant))
    }

    pub(crate) fn select_unique<'a, I>(&self, assets: I) -> Result<&'a str>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let matches = assets
            .into_iter()
            .filter(|asset| self.matches(asset))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [only] => Ok(*only),
            [] => Err(Error::Message(
                "no compatible release asset matches the semantic selector".to_owned(),
            )),
            _ => Err(Error::Message(format!(
                "semantic selector matches {} release assets; choose the release asset again",
                matches.len()
            ))),
        }
    }

    fn validate(&self, context: &str) -> Result<()> {
        validate_identifier(context, "asset product", &self.product)?;
        if let Some(variant) = &self.variant {
            validate_identifier(context, "asset variant", variant)?;
        }
        Ok(())
    }
}

fn release_version_boundary(stem: &str, release_tag: &str) -> Option<usize> {
    let tag = release_tag.trim().to_ascii_lowercase();
    let mut candidates = vec![tag.clone()];
    if let Some(version_start) = tag.find(|character: char| character.is_ascii_digit()) {
        let version = &tag[version_start..];
        candidates.push(version.to_owned());
        candidates.push(format!("v{version}"));
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.len()));
    candidates.dedup();
    candidates.into_iter().find_map(|candidate| {
        if candidate.is_empty() {
            return None;
        }
        stem.match_indices(&candidate)
            .filter_map(|(start, value)| {
                let end = start + value.len();
                let left_boundary = stem[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|character| !character.is_ascii_alphanumeric());
                let right_boundary = stem[end..]
                    .chars()
                    .next()
                    .is_none_or(|character| !character.is_ascii_alphanumeric());
                (left_boundary && right_boundary).then_some(start)
            })
            .max()
    })
}

fn contains_identifier(haystack: &str, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(&needle).any(|(start, value)| {
        let end = start + value.len();
        let left = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_ascii_alphanumeric());
        let right = haystack[end..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_ascii_alphanumeric());
        left && right
    })
}

fn validate_identifier(context: &str, label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.trim() != value
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(Error::InvalidConfig(format!(
            "{context} has an invalid {label} `{value}`"
        )));
    }
    Ok(())
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
    pub(crate) asset: AssetSelector,
    pub(crate) target_directory: PathBuf,
    #[serde(default, skip_serializing_if = "is_manual_release_update_policy")]
    pub(crate) update_policy: ReleaseUpdatePolicy,
    pub(crate) enabled: bool,
}

impl GithubReleaseMonitor {
    pub(crate) fn validate(&self) -> Result<()> {
        if !valid_github_release_monitor_name(&self.name) {
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
        self.asset
            .validate(&format!("GitHub release monitor `{}`", self.name))?;
        if !self.target_directory.is_absolute() {
            return Err(Error::InvalidConfig(format!(
                "GitHub release monitor `{}` target_directory must be an absolute path",
                self.name
            )));
        }
        if self.asset.format == ReleaseAssetFormat::Dmg {
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
        Ok(())
    }
}

pub(crate) fn valid_github_release_monitor_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
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
    Homebrew { formula: String },
    Npm { package: String },
    Pypi { package: String },
    CratesIo { package: String },
    GithubRelease { repository: String },
    GithubTag { repository: String },
}

impl LatestVersionSource {
    pub(crate) fn validate(&self, context: &str) -> Result<()> {
        let value = match self {
            Self::Homebrew { formula } => formula,
            Self::Npm { package } | Self::Pypi { package } | Self::CratesIo { package } => package,
            Self::GithubRelease { repository } | Self::GithubTag { repository } => repository,
        };
        if value.is_empty()
            || value.trim() != value
            || value.chars().any(char::is_whitespace)
            || value.chars().any(char::is_control)
        {
            return Err(Error::InvalidConfig(format!(
                "{context} has an invalid latest-version source identifier"
            )));
        }
        if matches!(self, Self::GithubRelease { .. } | Self::GithubTag { .. }) {
            let parts = value.split('/').collect::<Vec<_>>();
            if parts.len() != 2
                || parts.iter().any(|part| {
                    part.is_empty()
                        || !part.chars().all(|character| {
                            character.is_ascii_alphanumeric() || "-_.".contains(character)
                        })
                })
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

#[cfg(test)]
fn inferred_platforms(program: &str) -> &'static [&'static str] {
    match normalized_executable_name(program).as_str() {
        "brew" => &["macos", "linux"],
        "scoop" => &["windows"],
        _ => &[],
    }
}

impl CommandSpec {
    pub(crate) fn compile(&self, name: &str) -> Result<Tool> {
        validate_identifier("command declaration", "name", name)?;
        let tool = match self {
            Self::Package(spec) => {
                validate_package_name(name, &spec.package)?;
                validate_identifier(name, "executable", &spec.executable)?;
                if spec.probe_args.iter().any(|argument| argument.is_empty()) {
                    return Err(Error::InvalidConfig(format!(
                        "command `{name}` probe_args cannot contain an empty argument"
                    )));
                }
                let update = spec.manager.update_command(&spec.package);
                let (program, args) = split_user_command(name, "update", update)?;
                Tool {
                    program,
                    args,
                    probe: ToolProbe {
                        program: spec.executable.clone(),
                        args: spec.probe_args.clone(),
                    },
                    latest: Some(spec.manager.latest_source(&spec.package)),
                    update_version: spec.manager.update_version_command(&spec.package),
                    background: ToolBackground::Auto,
                    processes: vec![ProcessRule::wait(spec.executable.clone())],
                    lock_timeout_secs: default_lock_timeout_secs(),
                    retries: default_retries(),
                    retry_delay_secs: default_retry_delay_secs(),
                    platforms: spec.manager.platforms(),
                    resource_group: Some(spec.manager.resource_group().to_owned()),
                }
            }
            Self::Custom(spec) => {
                if matches!(
                    spec.latest.as_ref(),
                    Some(LatestVersionSource::Homebrew { .. })
                ) {
                    return Err(Error::InvalidConfig(format!(
                        "custom command `{name}` does not support a Homebrew latest-version source"
                    )));
                }
                let (program, args) = split_user_command(name, "update", spec.update.clone())?;
                let (probe_program, probe_args) =
                    split_user_command(name, "probe", spec.probe.clone())?;
                let resource_group = inferred_resource_group(&program).map(str::to_owned);
                Tool {
                    program,
                    args,
                    probe: ToolProbe {
                        program: probe_program.clone(),
                        args: probe_args,
                    },
                    latest: spec.latest.clone(),
                    update_version: None,
                    background: ToolBackground::Auto,
                    processes: vec![ProcessRule::wait(probe_program)],
                    lock_timeout_secs: default_lock_timeout_secs(),
                    retries: default_retries(),
                    retry_delay_secs: default_retry_delay_secs(),
                    platforms: Vec::new(),
                    resource_group,
                }
            }
        };
        if let Some(latest) = &tool.latest {
            latest.validate(&format!("command `{name}`"))?;
        }
        validate_update_version_template(name, tool.update_version.as_deref())?;
        Ok(tool)
    }

    pub(crate) fn custom(name: &str, update: Vec<String>) -> Self {
        Self::Custom(CustomCommandSpec {
            update,
            probe: vec![name.to_owned(), "--version".to_owned()],
            latest: None,
        })
    }
}

fn validate_package_name(context: &str, package: &str) -> Result<()> {
    if package.is_empty()
        || package.trim() != package
        || package.chars().any(char::is_whitespace)
        || package.chars().any(char::is_control)
    {
        return Err(Error::InvalidConfig(format!(
            "command `{context}` has an invalid package name"
        )));
    }
    Ok(())
}

impl GithubMonitorSpec {
    fn validate(&self, name: &str) -> Result<()> {
        validate_identifier("GitHub monitor declaration", "name", name)?;
        LatestVersionSource::GithubRelease {
            repository: self.repository.clone(),
        }
        .validate(&format!("GitHub monitor `{name}`"))?;
        self.asset.validate(&format!("GitHub monitor `{name}`"))?;
        match (&self.install, self.asset.format) {
            (GithubInstallSpec::UserDirectory, ReleaseAssetFormat::Dmg) => {
                return Err(Error::InvalidConfig(format!(
                    "GitHub monitor `{name}` must install a DMG as a macOS application"
                )));
            }
            (GithubInstallSpec::MacosApplication { application }, ReleaseAssetFormat::Dmg) => {
                if application.trim() != application
                    || !application.ends_with(".app")
                    || Path::new(application)
                        .file_name()
                        .and_then(|part| part.to_str())
                        != Some(application)
                {
                    return Err(Error::InvalidConfig(format!(
                        "GitHub monitor `{name}` application must be one .app directory name"
                    )));
                }
                if self.asset.os != AssetOperatingSystem::Macos {
                    return Err(Error::InvalidConfig(format!(
                        "GitHub monitor `{name}` DMG asset must target macOS"
                    )));
                }
            }
            (GithubInstallSpec::MacosApplication { .. }, _) => {
                return Err(Error::InvalidConfig(format!(
                    "GitHub monitor `{name}` macOS application install requires a DMG asset"
                )));
            }
            (GithubInstallSpec::UserDirectory, _) => {}
        }
        Ok(())
    }

    fn compile(&self, name: &str, install_root: &Path) -> Result<GithubReleaseMonitor> {
        self.validate(name)?;
        let target_directory = match &self.install {
            GithubInstallSpec::UserDirectory => install_root.join(name),
            GithubInstallSpec::MacosApplication { application } => {
                Path::new("/Applications").join(application)
            }
        };
        if !target_directory.is_absolute() {
            return Err(Error::InvalidConfig(format!(
                "GitHub install root must be absolute: {}",
                install_root.display()
            )));
        }
        Ok(GithubReleaseMonitor {
            name: name.to_owned(),
            repository: self.repository.clone(),
            asset: self.asset.clone(),
            target_directory,
            update_policy: self.update_policy,
            enabled: self.enabled,
        })
    }
}

pub(crate) const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const DEFAULT_MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const DEFAULT_MAX_EXTRACTED_FILES: usize = 50_000;

impl UserConfig {
    /// Returns an empty user manifest ready to receive custom tools.
    pub fn empty() -> Self {
        Self {
            commands: BTreeMap::new(),
            github: GithubConfig::default(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.github.is_empty()
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
        let config: Self = toml::from_str(contents).map_err(|error| {
            Error::InvalidConfig(format!(
                "user TOML must use the current [commands.<name>] and [github.monitors.<name>] schema; legacy user formats are unsupported: {error}"
            ))
        })?;
        config.validate_and_compile()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        for (name, command) in &self.commands {
            command.compile(name)?;
        }
        for (name, monitor) in &self.github.monitors {
            monitor.validate(name)?;
        }
        Ok(())
    }

    fn validate_and_compile(&self) -> Result<()> {
        self.validate()?;
        let install_root = std::env::current_dir()?.join(".dvup-github-validation");
        self.clone().resolve_with_install_root(&install_root)?;
        Ok(())
    }

    pub(crate) fn resolve_with_install_root(self, install_root: &Path) -> Result<Config> {
        let mut tools = BTreeMap::new();
        for (name, command) in self.commands {
            tools.insert(name.clone(), command.compile(&name)?);
        }
        let mut monitors = Vec::with_capacity(self.github.monitors.len());
        for (name, monitor) in self.github.monitors {
            monitors.push(monitor.compile(&name, install_root)?);
        }
        let config = Config {
            tools,
            github: GithubMonitorConfig { monitors },
        };
        config.validate_inner(false)?;
        Ok(config)
    }

    pub(crate) fn save_command(
        &mut self,
        path: &Path,
        name: String,
        command: CommandSpec,
    ) -> Result<()> {
        command.compile(&name)?;
        self.commands.insert(name, command);
        self.save(path)
    }

    pub(crate) fn save_github_monitor(
        &mut self,
        path: &Path,
        name: String,
        monitor: GithubMonitorSpec,
    ) -> Result<()> {
        monitor.validate(&name)?;
        self.github.monitors.insert(name, monitor);
        self.save(path)
    }

    /// Validates and atomically saves a user-authored manifest.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate_and_compile()?;
        if self.is_empty() {
            return remove_user_config(path);
        }
        write_atomic(path, toml::to_string(self)?.as_bytes())
    }

    /// Validates and atomically saves TOML while preserving editor formatting.
    pub fn save_text(path: &Path, contents: &str) -> Result<()> {
        let config = Self::parse(contents)?;
        if config.is_empty() {
            return remove_user_config(path);
        }
        write_atomic(path, contents.as_bytes())
    }

    /// Validates and atomically writes the comment-only starter created by `dvup init`.
    pub(crate) fn save_template_text(path: &Path, contents: &str) -> Result<()> {
        Self::parse(contents)?;
        write_atomic(path, contents.as_bytes())
    }
}

fn remove_user_config(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
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
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".dvup-config.")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    persist_config_file(temporary, path)
}

#[cfg(windows)]
fn persist_config_file(mut temporary: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    let mut retry = 0_u32;
    loop {
        match temporary.persist(path) {
            Ok(_) => return Ok(()),
            Err(error) => {
                if retry < 4 && matches!(error.error.raw_os_error(), Some(5 | 32)) {
                    temporary = error.file;
                    retry += 1;
                    std::thread::sleep(std::time::Duration::from_millis(u64::from(retry) * 25));
                    continue;
                }
                return Err(Error::ConfigWrite {
                    path: path.to_path_buf(),
                    source: error.error,
                });
            }
        }
    }
}

#[cfg(not(windows))]
fn persist_config_file(temporary: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| Error::ConfigWrite {
            path: path.to_path_buf(),
            source: error.error,
        })
}

fn normalize_process_name(name: &str) -> String {
    let normalized = name.trim().to_lowercase();
    normalized
        .strip_suffix(".exe")
        .unwrap_or(&normalized)
        .to_owned()
}

pub(crate) const fn default_lock_timeout_secs() -> u64 {
    86_400
}

pub(crate) const fn default_retries() -> u32 {
    8
}

pub(crate) const fn default_retry_delay_secs() -> u64 {
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
    fn package_declarations_compile_all_supported_managers_deterministically() {
        let cases = [
            (
                PackageManager::Homebrew,
                vec!["brew", "upgrade", "example"],
                None,
                Some("homebrew"),
                vec!["macos", "linux"],
            ),
            (
                PackageManager::Npm,
                vec!["npm", "install", "--global", "example@latest"],
                Some(vec!["npm", "install", "--global", "example@{version}"]),
                Some("node-global"),
                vec![],
            ),
            (
                PackageManager::Pnpm,
                vec!["pnpm", "add", "--global", "example@latest"],
                Some(vec!["pnpm", "add", "--global", "example@{version}"]),
                Some("node-global"),
                vec![],
            ),
            (
                PackageManager::Cargo,
                vec!["cargo", "install", "example"],
                Some(vec![
                    "cargo",
                    "install",
                    "example",
                    "--version",
                    "{version}",
                ]),
                Some("cargo-global"),
                vec![],
            ),
            (
                PackageManager::Pipx,
                vec!["pipx", "upgrade", "example"],
                Some(vec!["pipx", "install", "--force", "example=={version}"]),
                Some("python-global"),
                vec![],
            ),
            (
                PackageManager::Uv,
                vec!["uv", "tool", "upgrade", "example"],
                Some(vec![
                    "uv",
                    "tool",
                    "install",
                    "--force",
                    "example=={version}",
                ]),
                Some("python-global"),
                vec![],
            ),
        ];

        for (manager, update, update_version, resource_group, platforms) in cases {
            let command = CommandSpec::Package(PackageCommandSpec {
                manager,
                package: "example".to_owned(),
                executable: "example".to_owned(),
                probe_args: vec!["--version".to_owned()],
            });
            let tool = command
                .compile("example")
                .expect("compile package declaration");
            let actual_update = std::iter::once(tool.program.as_str())
                .chain(tool.args.iter().map(String::as_str))
                .collect::<Vec<_>>();

            assert_eq!(actual_update, update, "manager: {manager:?}");
            assert_eq!(
                tool.update_version
                    .as_ref()
                    .map(|parts| { parts.iter().map(String::as_str).collect::<Vec<_>>() }),
                update_version,
                "manager: {manager:?}"
            );
            assert_eq!(
                tool.resource_group.as_deref(),
                resource_group,
                "manager: {manager:?}"
            );
            assert_eq!(tool.platforms, platforms, "manager: {manager:?}");
            let expected_latest = match manager {
                PackageManager::Homebrew => LatestVersionSource::Homebrew {
                    formula: "example".to_owned(),
                },
                PackageManager::Npm | PackageManager::Pnpm => LatestVersionSource::Npm {
                    package: "example".to_owned(),
                },
                PackageManager::Cargo => LatestVersionSource::CratesIo {
                    package: "example".to_owned(),
                },
                PackageManager::Pipx | PackageManager::Uv => LatestVersionSource::Pypi {
                    package: "example".to_owned(),
                },
            };
            assert_eq!(tool.latest, Some(expected_latest), "manager: {manager:?}");
            assert_eq!(tool.background, ToolBackground::Auto);
            assert_eq!(tool.processes.len(), 1);
            assert_eq!(tool.processes[0].name, "example");
            assert_eq!(tool.processes[0].action, ProcessAction::Wait);
            assert_eq!(tool.lock_timeout_secs, default_lock_timeout_secs());
            assert_eq!(tool.retries, default_retries());
            assert_eq!(tool.retry_delay_secs, default_retry_delay_secs());
        }
    }

    #[test]
    fn new_user_manifest_round_trips_and_rejects_removed_formats() {
        let input = r#"
[commands.codegraph]
type = "package"
manager = "npm"
package = "@colbymchenry/codegraph"
executable = "codegraph"

[commands.deno]
type = "custom"
update = ["deno", "upgrade"]
probe = ["deno", "--version"]
latest = { provider = "github_release", repository = "denoland/deno" }

[github.monitors.ripgrep]
repository = "BurntSushi/ripgrep"
asset = { product = "ripgrep", os = "macos", arch = "aarch64", format = "tar_gz" }
install = { type = "user_directory" }
"#;
        let user = UserConfig::parse(input).expect("parse new user manifest");
        assert_eq!(user.commands.len(), 2);
        assert_eq!(user.github.monitors.len(), 1);
        let encoded = toml::to_string_pretty(&user).expect("serialize new user manifest");
        assert!(!encoded.contains("probe_args"));
        assert!(UserConfig::parse(&encoded).is_ok());

        for legacy in [
            "[tools.old]\nupdate = [\"old\", \"update\"]\nprobe = [\"old\", \"--version\"]\n",
            "[[github.monitors]]\nname = \"old\"\nrepository = \"owner/repo\"\n",
            "[github.monitors.old]\nrepository = \"owner/repo\"\nasset_regex = \"old\"\n",
        ] {
            let error = UserConfig::parse(legacy).expect_err("legacy format must fail");
            assert!(
                error
                    .to_string()
                    .contains("legacy user formats are unsupported"),
                "legacy: {legacy}"
            );
        }
    }

    #[test]
    fn custom_declarations_compile_each_optional_official_source_without_target_versions() {
        let sources = [
            LatestVersionSource::Npm {
                package: "example".to_owned(),
            },
            LatestVersionSource::Pypi {
                package: "example".to_owned(),
            },
            LatestVersionSource::CratesIo {
                package: "example".to_owned(),
            },
            LatestVersionSource::GithubRelease {
                repository: "owner/example".to_owned(),
            },
            LatestVersionSource::GithubTag {
                repository: "owner/example".to_owned(),
            },
        ];

        for source in sources {
            let tool = CommandSpec::Custom(CustomCommandSpec {
                update: vec!["example".to_owned(), "upgrade".to_owned()],
                probe: vec!["example".to_owned(), "--version".to_owned()],
                latest: Some(source.clone()),
            })
            .compile("example")
            .expect("compile custom declaration");

            assert_eq!(tool.latest, Some(source));
            assert!(tool.update_version.is_none());
        }

        assert!(
            CommandSpec::Custom(CustomCommandSpec {
                update: vec!["example".to_owned(), "upgrade".to_owned()],
                probe: vec!["example".to_owned(), "--version".to_owned()],
                latest: Some(LatestVersionSource::Homebrew {
                    formula: "example".to_owned(),
                }),
            })
            .compile("example")
            .is_err()
        );
    }

    #[test]
    fn concise_declarations_reject_missing_required_identity_and_commands() {
        for invalid in [
            "[commands.example]\ntype = \"package\"\npackage = \"example\"\nexecutable = \"example\"\n",
            "[commands.example]\ntype = \"package\"\nmanager = \"cargo\"\nexecutable = \"example\"\n",
            "[commands.example]\ntype = \"package\"\nmanager = \"cargo\"\npackage = \"example\"\n",
            "[commands.example]\ntype = \"custom\"\nprobe = [\"example\", \"--version\"]\n",
            "[commands.example]\ntype = \"custom\"\nupdate = [\"example\", \"upgrade\"]\n",
        ] {
            assert!(
                UserConfig::parse(invalid).is_err(),
                "invalid declaration: {invalid}"
            );
        }
    }

    #[test]
    fn semantic_asset_selector_handles_aliases_and_requires_one_match() {
        let selector = AssetSelector {
            product: "ripgrep".to_owned(),
            os: AssetOperatingSystem::Macos,
            arch: AssetArchitecture::Aarch64,
            format: ReleaseAssetFormat::TarGz,
            variant: None,
        };
        let assets = [
            "ripgrep-14.1.1-aarch64-apple-darwin.tar.gz",
            "ripgrep-14.1.1-x86_64-apple-darwin.tar.gz",
            "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz",
        ];

        assert_eq!(
            selector.select_unique(assets).expect("one matching asset"),
            assets[0]
        );
        assert!(
            selector
                .select_unique([assets[0], "ripgrep-14.1.1-arm64-macos.tgz"])
                .is_err()
        );
        assert!(
            selector
                .select_unique(["ripgrep-14.1.1-amd64-windows.zip"])
                .is_err()
        );
    }

    #[test]
    fn asset_selector_infers_numeric_and_hyphenated_product_names() {
        let os = AssetOperatingSystem::current().expect("supported test OS");
        let arch = AssetArchitecture::current().expect("supported test architecture");
        let os_name = os.aliases()[0];
        let arch_name = arch.aliases()[0];
        let numeric = format!("7zip-24.09-{os_name}-{arch_name}.tar.gz");
        let without_version = format!("my-tool-{os_name}-{arch_name}.zip");

        assert_eq!(
            AssetSelector::from_release_asset(&numeric, "v24.09")
                .expect("numeric product selector")
                .product,
            "7zip"
        );
        assert_eq!(
            AssetSelector::from_release_asset(&without_version, "v1.0.0")
                .expect("hyphenated product selector")
                .product,
            "my-tool"
        );
    }

    #[test]
    fn asset_selector_uses_the_release_tag_without_capturing_v_prefixed_versions() {
        let os = AssetOperatingSystem::current().expect("supported test OS");
        let arch = AssetArchitecture::current().expect("supported test architecture");
        let os_name = os.aliases()[0];
        let arch_name = arch.aliases()[0];
        let selected = format!("python-3-launcher-v10.2.0-{os_name}-{arch_name}.tar.gz");
        let future = format!("python-3-launcher-v11.0.0-{os_name}-{arch_name}.tar.gz");

        let selector = AssetSelector::from_release_asset(&selected, "v10.2.0")
            .expect("selector from tagged release asset");

        assert_eq!(selector.product, "python-3-launcher");
        assert!(selector.matches(&future));
    }

    #[test]
    fn semantic_selector_matches_os_arch_format_and_libc_aliases() {
        let mac_arm = AssetSelector {
            product: "tool".to_owned(),
            os: AssetOperatingSystem::Macos,
            arch: AssetArchitecture::Aarch64,
            format: ReleaseAssetFormat::TarGz,
            variant: None,
        };
        for asset in [
            "tool-arm64-macos.tgz",
            "tool-aarch64-darwin.tar.gz",
            "tool-arm64-osx.tar.gz",
            "tool-aarch64-apple-darwin.tgz",
        ] {
            assert!(mac_arm.matches(asset), "asset: {asset}");
        }

        let windows_x64 = AssetSelector {
            product: "tool".to_owned(),
            os: AssetOperatingSystem::Windows,
            arch: AssetArchitecture::X86_64,
            format: ReleaseAssetFormat::Zip,
            variant: None,
        };
        for asset in [
            "tool-x86_64-windows.zip",
            "tool-amd64-win32.zip",
            "tool-x64-win64.zip",
            "tool-x86-64-pc-windows.zip",
        ] {
            assert!(windows_x64.matches(asset), "asset: {asset}");
        }

        let linux_musl = AssetSelector {
            product: "tool".to_owned(),
            os: AssetOperatingSystem::Linux,
            arch: AssetArchitecture::X86_64,
            format: ReleaseAssetFormat::TarGz,
            variant: Some("musl".to_owned()),
        };
        assert!(linux_musl.matches("tool-amd64-unknown-linux-musl.tar.gz"));
        assert!(!linux_musl.matches("tool-amd64-unknown-linux-gnu.tar.gz"));
    }

    #[test]
    fn command_and_github_save_interfaces_only_change_their_own_partitions() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("dvup_custom.toml");
        let mut config = UserConfig::empty();
        config
            .save_github_monitor(
                &path,
                "shared".to_owned(),
                GithubMonitorSpec {
                    repository: "owner/repository".to_owned(),
                    asset: AssetSelector {
                        product: "shared".to_owned(),
                        os: AssetOperatingSystem::Linux,
                        arch: AssetArchitecture::X86_64,
                        format: ReleaseAssetFormat::TarGz,
                        variant: Some("gnu".to_owned()),
                    },
                    install: GithubInstallSpec::UserDirectory,
                    update_policy: ReleaseUpdatePolicy::Manual,
                    enabled: true,
                },
            )
            .expect("save GitHub monitor");
        config
            .save_command(
                &path,
                "shared".to_owned(),
                CommandSpec::Custom(CustomCommandSpec {
                    update: vec!["shared".to_owned(), "upgrade".to_owned()],
                    probe: vec!["shared".to_owned(), "--version".to_owned()],
                    latest: None,
                }),
            )
            .expect("save same-named command");

        let reloaded = UserConfig::load(&path).expect("reload both partitions");
        assert_eq!(reloaded.commands.len(), 1);
        assert_eq!(reloaded.github.monitors.len(), 1);
        assert!(reloaded.commands.contains_key("shared"));
        assert!(reloaded.github.monitors.contains_key("shared"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dmg_declaration_compiles_to_the_fixed_applications_target() {
        let monitor = GithubMonitorSpec {
            repository: "owner/example".to_owned(),
            asset: AssetSelector {
                product: "example".to_owned(),
                os: AssetOperatingSystem::Macos,
                arch: AssetArchitecture::Aarch64,
                format: ReleaseAssetFormat::Dmg,
                variant: None,
            },
            install: GithubInstallSpec::MacosApplication {
                application: "Example.app".to_owned(),
            },
            update_policy: ReleaseUpdatePolicy::Manual,
            enabled: true,
        };

        let runtime = monitor
            .compile("example", Path::new("/tmp/dvup-test"))
            .expect("compile DMG declaration");

        assert_eq!(
            runtime.target_directory,
            Path::new("/Applications/Example.app")
        );
    }

    #[test]
    fn saving_an_empty_user_config_removes_the_file() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("dvup_custom.toml");
        fs::write(&path, "stale").expect("seed file");

        UserConfig::empty()
            .save(&path)
            .expect("remove empty config");

        assert!(!path.exists());
    }

    #[test]
    fn saving_semantically_empty_editor_text_removes_the_file() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("dvup_custom.toml");
        fs::write(&path, "[commands.old]\ntype = \"custom\"\nupdate = [\"old\"]\nprobe = [\"old\", \"--version\"]\n")
            .expect("seed user config");

        UserConfig::save_text(&path, "# no declarations remain\n").expect("save empty editor text");

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_atomic_replace_preserves_the_previous_configuration() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("dvup_custom.toml");
        fs::write(&path, "old configuration").expect("seed config");
        let mut replacement = tempfile::Builder::new()
            .prefix(".dvup-config.")
            .tempfile_in(temporary.path())
            .expect("replacement file");
        replacement
            .write_all(b"new configuration")
            .expect("write replacement");
        replacement.as_file().sync_all().expect("sync replacement");
        let original_permissions = fs::metadata(temporary.path())
            .expect("directory metadata")
            .permissions();
        let mut locked_permissions = original_permissions.clone();
        locked_permissions.set_mode(0o500);
        fs::set_permissions(temporary.path(), locked_permissions).expect("lock directory");

        let result = persist_config_file(replacement, &path);

        fs::set_permissions(temporary.path(), original_permissions).expect("unlock directory");
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("read retained config"),
            "old configuration"
        );
    }
}
