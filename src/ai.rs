use std::{
    collections::HashSet,
    env,
    path::{Component, Path},
    time::Duration,
};

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    config::{
        GithubReleaseMonitor, ReleaseAssetFormat, ReleaseUpdatePolicy, UserTool,
        valid_github_release_monitor_name,
    },
    credential,
    error::{Error, Result},
    settings::{AiSettings, NetworkSettings, ProxyMode},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const CONNECTION_TEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_AI_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_GITHUB_RELEASE_BYTES: u64 = 1024 * 1024;
const MAX_GENERATED_NAME_BYTES: usize = 128;
const MAX_GENERATED_COMMAND_BYTES: usize = 4 * 1024;
const MAX_GENERATED_FIELD_BYTES: usize = 4 * 1024;
const USER_AGENT: &str = concat!("dvup/", env!("CARGO_PKG_VERSION"));
const PACKAGE_MANAGERS: &[&str] = &[
    "brew", "scoop", "winget", "apt", "dnf", "pacman", "npm", "pnpm", "bun", "cargo", "pipx", "uv",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SystemContext {
    pub(crate) os: &'static str,
    pub(crate) arch: &'static str,
    pub(crate) available_package_managers: Vec<&'static str>,
    pub(crate) install_root: String,
}

impl SystemContext {
    pub(crate) fn detect(install_root: &Path) -> Self {
        Self {
            os: env::consts::OS,
            arch: env::consts::ARCH,
            available_package_managers: PACKAGE_MANAGERS
                .iter()
                .copied()
                .filter(|program| program_on_path(program))
                .collect(),
            install_root: install_root.display().to_string(),
        }
    }

    fn prompt(&self) -> String {
        format!(
            "operating_system={}\narchitecture={}\navailable_package_managers={}\nuser_writable_install_root={}",
            self.os,
            self.arch,
            self.available_package_managers.join(","),
            self.install_root,
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GeneratedCommand {
    pub(crate) name: String,
    pub(crate) tool: UserTool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedCommandEnvelope {
    name: String,
    tool: serde_json::Value,
}

impl<'de> Deserialize<'de> for GeneratedCommand {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        const REQUIRED_TOOL_FIELDS: &[&str] = &[
            "update",
            "probe",
            "latest",
            "update_version",
            "background",
            "wait_for",
            "processes",
            "lock_timeout_secs",
            "retries",
            "retry_delay_secs",
            "platforms",
            "resource_group",
        ];

        let envelope = GeneratedCommandEnvelope::deserialize(deserializer)?;
        let object = envelope
            .tool
            .as_object()
            .ok_or_else(|| D::Error::custom("AI tool configuration must be a JSON object"))?;
        for &field in REQUIRED_TOOL_FIELDS {
            if !object.contains_key(field) {
                return Err(D::Error::missing_field(field));
            }
        }
        let tool = UserTool::deserialize(envelope.tool).map_err(D::Error::custom)?;
        Ok(Self {
            name: envelope.name,
            tool,
        })
    }
}

impl GeneratedCommand {
    pub(crate) fn into_user_tool(self) -> (String, UserTool) {
        (self.name, self.tool)
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: serde_json::Value,
}

#[derive(Deserialize)]
struct GithubLatestRelease {
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Deserialize)]
struct GithubReleaseAsset {
    name: String,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

pub(crate) fn generate_command(
    settings: &AiSettings,
    network: &NetworkSettings,
    context: &SystemContext,
    name_hint: &str,
    configuration_hint: &str,
) -> Result<GeneratedCommand> {
    if !settings.configured() {
        return Err(Error::Message(
            "AI generation is disabled or incomplete".to_owned(),
        ));
    }
    let system = concat!(
        "You generate one complete dvup custom-tool definition. Return one JSON object only with ",
        "exactly two properties: name (string) and tool (object). The tool object must have exactly ",
        "these properties: update (non-empty string array), probe (non-empty string array), latest ",
        "(null or an object with provider npm, pypi, crates_io, ",
        "github_release, or github_tag plus package/repository), update_version (null or a string ",
        "array containing exactly one {version}), background (auto or always), wait_for (null or a ",
        "string array), processes (array of objects with name, optional command_contains, action ",
        "wait/terminate/fail, and terminate_grace_secs), lock_timeout_secs (positive integer), ",
        "retries (non-negative integer), retry_delay_secs (non-negative integer), platforms (array ",
        "containing only windows, macos, or linux), and resource_group (null or string). The update ",
        "array must be one direct executable invocation, never a shell pipeline, redirection, sudo, ",
        "or an installer download script. The probe must print the installed version. Prefer an ",
        "available package manager and supply latest/update_version when the ecosystem supports ",
        "them. Do not include markdown or extra properties."
    );
    let user = format!(
        "Generate or improve this custom tool.\n{}\nname_hint={}\nconfiguration_hint={}",
        context.prompt(),
        name_hint.trim(),
        configuration_hint.trim(),
    );
    let generated = chat_json::<GeneratedCommand>(settings, network, system, &user)?;
    if generated.name.is_empty()
        || generated.name.trim() != generated.name
        || generated.tool.update.is_empty()
        || generated.tool.probe.is_empty()
        || generated.name.len() > MAX_GENERATED_NAME_BYTES
        || generated
            .tool
            .update
            .iter()
            .chain(&generated.tool.probe)
            .chain(generated.tool.update_version.iter().flatten())
            .any(|value| value.len() > MAX_GENERATED_COMMAND_BYTES)
    {
        return Err(Error::Message(
            "AI returned an empty, whitespace-padded, or oversized custom-tool configuration"
                .to_owned(),
        ));
    }
    generated.tool.validate_for_name(&generated.name)?;
    Ok(generated)
}

pub(crate) fn generate_github_monitor(
    settings: &AiSettings,
    network: &NetworkSettings,
    github_api_key: Option<&str>,
    context: &SystemContext,
    repository_hint: &str,
) -> Result<GithubReleaseMonitor> {
    if !settings.configured() {
        return Err(Error::Message(
            "AI generation is disabled or incomplete".to_owned(),
        ));
    }
    let repository = normalize_github_repository(repository_hint)?;
    let assets = latest_release_assets(network, github_api_key, &repository)?;
    if assets.is_empty() {
        return Err(Error::Message(format!(
            "GitHub latest release for {repository} contains no assets"
        )));
    }
    let system = concat!(
        "You generate one dvup GitHub Release monitor. Return one JSON object only with exactly ",
        "these properties: name (string), repository (owner/repo string), asset_regex (Rust regex ",
        "string matching exactly one supplied asset), target_directory (absolute string path; under ",
        "the supplied user_writable_install_root for file, zip, and tar_gz, or ",
        "/Applications/<Application>.app for dmg), format (file, zip, tar_gz, or dmg), ",
        "update_policy (manual), cleanup_installer (boolean), max_download_bytes (integer), ",
        "max_extracted_bytes (integer; 0 for file), max_extracted_files (integer; 0 for file), ",
        "strip_components (integer), and enabled (true). Select only an asset compatible with the ",
        "supplied operating system and architecture. Escape the exact stable filename shape into ",
        "an anchored regex while allowing release version changes. Do not include markdown or ",
        "extra properties."
    );
    let user = format!(
        "Generate a monitor for this repository.\n{}\nrepository={}\nlatest_release_assets={}",
        context.prompt(),
        repository,
        serde_json::to_string(&assets)?,
    );
    let monitor = chat_json::<GithubReleaseMonitor>(settings, network, system, &user)?;
    validate_generated_monitor(monitor, &repository, context, &assets)
}

pub(crate) fn test_connection(settings: &AiSettings, network: &NetworkSettings) -> Result<()> {
    let response = chat_text_with_timeout(
        settings,
        network,
        "You are checking an API connection. Reply with OK and nothing else.",
        "OK",
        CONNECTION_TEST_TIMEOUT,
    )?;
    if response.trim().is_empty() {
        return Err(Error::Message(
            "AI connection returned an empty response".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn list_models(settings: &AiSettings, network: &NetworkSettings) -> Result<Vec<String>> {
    settings.validate_endpoint()?;
    let base_url = settings
        .base_url
        .as_deref()
        .expect("validated AI endpoint has a base URL");
    let api_key = credential::ai_api_key(settings.encrypted_api_key.as_deref())?;
    let endpoint = models_endpoint(base_url);
    let agent = network_agent(network, CONNECTION_TEST_TIMEOUT)?;
    let request = agent
        .get(&endpoint)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json");
    let request = if let Some(api_key) = api_key.as_deref() {
        request.header("Authorization", &format!("Bearer {api_key}"))
    } else {
        request
    };
    let mut response = request
        .call()
        .map_err(|error| Error::Message(format!("AI model list request failed: {error}")))?;
    let response = response
        .body_mut()
        .with_config()
        .limit(MAX_AI_RESPONSE_BYTES)
        .read_json::<ModelsResponse>()
        .map_err(|error| Error::Message(format!("AI returned an invalid model list: {error}")))?;
    let mut seen = HashSet::new();
    let models = response
        .data
        .into_iter()
        .map(|model| model.id)
        .filter(|id| !id.is_empty() && id.trim() == id)
        .filter(|id| seen.insert(id.clone()))
        .collect::<Vec<_>>();
    if models.is_empty() {
        return Err(Error::Message(
            "AI endpoint returned no selectable models".to_owned(),
        ));
    }
    Ok(models)
}

fn validate_generated_monitor(
    mut monitor: GithubReleaseMonitor,
    repository: &str,
    context: &SystemContext,
    assets: &[String],
) -> Result<GithubReleaseMonitor> {
    if !valid_github_release_monitor_name(&monitor.name) {
        monitor.name = repository
            .rsplit_once('/')
            .map(|(_, name)| name)
            .unwrap_or(repository)
            .to_owned();
    }
    normalize_generated_target_directory(&mut monitor);
    monitor.validate()?;
    if monitor.repository != *repository {
        return Err(Error::Message(format!(
            "AI changed repository {repository} to {}",
            monitor.repository
        )));
    }
    if monitor.update_policy != ReleaseUpdatePolicy::Manual || !monitor.enabled {
        return Err(Error::Message(
            "AI monitor must be enabled with manual update policy".to_owned(),
        ));
    }
    if monitor.name.len() > MAX_GENERATED_NAME_BYTES
        || monitor.repository.len() > MAX_GENERATED_FIELD_BYTES
        || monitor.asset_regex.len() > MAX_GENERATED_FIELD_BYTES
        || monitor.target_directory.as_os_str().len() > MAX_GENERATED_FIELD_BYTES
    {
        return Err(Error::Message(
            "AI monitor contains an oversized text field".to_owned(),
        ));
    }
    let install_root = Path::new(&context.install_root);
    let contains_parent_traversal = monitor
        .target_directory
        .components()
        .any(|component| component == Component::ParentDir);
    if monitor.format == ReleaseAssetFormat::Dmg {
        if monitor.target_directory.parent() != Some(macos_applications_directory())
            || contains_parent_traversal
        {
            return Err(Error::Message(
                "AI DMG target must be /Applications/<Application>.app".to_owned(),
            ));
        }
    } else if monitor.target_directory == install_root
        || !monitor.target_directory.starts_with(install_root)
        || contains_parent_traversal
    {
        return Err(Error::Message(format!(
            "AI target directory must be a child of {} without parent traversal",
            install_root.display()
        )));
    }
    if !monitor.asset_regex.starts_with('^') || !monitor.asset_regex.ends_with('$') {
        return Err(Error::Message(
            "AI asset regex must be anchored with ^ and $".to_owned(),
        ));
    }
    let regex = Regex::new(&monitor.asset_regex)
        .map_err(|error| Error::Message(format!("AI returned an invalid asset regex: {error}")))?;
    let matched = assets
        .iter()
        .filter(|asset| regex.is_match(asset))
        .collect::<Vec<_>>();
    if matched.len() != 1 {
        return Err(Error::Message(format!(
            "AI asset regex matched {} latest-release assets instead of exactly one",
            matched.len()
        )));
    }
    let asset = matched[0].to_ascii_lowercase();
    if !asset_matches_system(&asset, context.os, context.arch) {
        return Err(Error::Message(format!(
            "AI selected asset {} for incompatible system {}/{}",
            matched[0], context.os, context.arch
        )));
    }
    let format_matches = match monitor.format {
        ReleaseAssetFormat::File => {
            !asset.ends_with(".zip")
                && !asset.ends_with(".tar.gz")
                && !asset.ends_with(".tgz")
                && !asset.ends_with(".dmg")
        }
        ReleaseAssetFormat::Zip => asset.ends_with(".zip"),
        ReleaseAssetFormat::TarGz => asset.ends_with(".tar.gz") || asset.ends_with(".tgz"),
        ReleaseAssetFormat::Dmg => asset.ends_with(".dmg"),
    };
    if !format_matches {
        return Err(Error::Message(format!(
            "AI selected format {} for incompatible asset {}",
            release_format_name(monitor.format),
            matched[0]
        )));
    }
    Ok(monitor)
}

fn normalize_generated_target_directory(monitor: &mut GithubReleaseMonitor) {
    if monitor.format != ReleaseAssetFormat::Dmg {
        return;
    }
    let Some(bundle_name) = monitor.target_directory.file_name().filter(|name| {
        Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("app")
    }) else {
        return;
    };
    monitor.target_directory = macos_applications_directory().join(bundle_name);
}

fn macos_applications_directory() -> &'static Path {
    Path::new("/Applications")
}

fn asset_matches_system(asset: &str, os: &str, arch: &str) -> bool {
    const WINDOWS_MARKERS: &[&str] = &["windows", "win32", "win64", "mingw", "msvc"];
    const MACOS_MARKERS: &[&str] = &["macos", "darwin", "osx"];
    const LINUX_MARKERS: &[&str] = &["linux", "musl"];
    const FREEBSD_MARKERS: &[&str] = &["freebsd"];
    const X86_64_MARKERS: &[&str] = &["x86_64", "x86-64", "amd64", "x64"];
    const AARCH64_MARKERS: &[&str] = &["aarch64", "arm64"];
    const X86_MARKERS: &[&str] = &["i686", "i386"];
    const ARM_MARKERS: &[&str] = &["armv7", "armhf"];

    let os_groups = [
        ("windows", WINDOWS_MARKERS),
        ("macos", MACOS_MARKERS),
        ("linux", LINUX_MARKERS),
        ("freebsd", FREEBSD_MARKERS),
    ];
    let arch_groups = [
        ("x86_64", X86_64_MARKERS),
        ("aarch64", AARCH64_MARKERS),
        ("x86", X86_MARKERS),
        ("arm", ARM_MARKERS),
    ];
    let has_known_os = os_groups
        .iter()
        .any(|(_, markers)| markers.iter().any(|marker| asset.contains(marker)));
    let has_current_os = os_groups
        .iter()
        .find(|(name, _)| *name == os)
        .is_some_and(|(_, markers)| markers.iter().any(|marker| asset.contains(marker)));
    let normalized_arch = match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        value => value,
    };
    let has_known_arch = arch_groups
        .iter()
        .any(|(_, markers)| markers.iter().any(|marker| asset.contains(marker)));
    let has_current_arch = arch_groups
        .iter()
        .find(|(name, _)| *name == normalized_arch)
        .is_some_and(|(_, markers)| markers.iter().any(|marker| asset.contains(marker)));

    (!has_known_os || has_current_os) && (!has_known_arch || has_current_arch)
}

fn release_format_name(format: ReleaseAssetFormat) -> &'static str {
    match format {
        ReleaseAssetFormat::File => "file",
        ReleaseAssetFormat::Zip => "zip",
        ReleaseAssetFormat::TarGz => "tar_gz",
        ReleaseAssetFormat::Dmg => "dmg",
    }
}

fn chat_json<T: for<'de> Deserialize<'de>>(
    settings: &AiSettings,
    network: &NetworkSettings,
    system: &str,
    user: &str,
) -> Result<T> {
    let text = chat_text(settings, network, system, user)?;
    let json = extract_json_object(&text)?;
    serde_json::from_str(json)
        .map_err(|error| Error::Message(format!("AI configuration is invalid: {error}")))
}

fn chat_text(
    settings: &AiSettings,
    network: &NetworkSettings,
    system: &str,
    user: &str,
) -> Result<String> {
    chat_text_with_timeout(settings, network, system, user, REQUEST_TIMEOUT)
}

fn chat_text_with_timeout(
    settings: &AiSettings,
    network: &NetworkSettings,
    system: &str,
    user: &str,
    timeout: Duration,
) -> Result<String> {
    settings.validate()?;
    let base_url = settings
        .base_url
        .as_deref()
        .ok_or_else(|| Error::Message("AI generation is not configured".to_owned()))?;
    let model = settings
        .model
        .as_deref()
        .ok_or_else(|| Error::Message("AI generation is not configured".to_owned()))?;
    let api_key = credential::ai_api_key(settings.encrypted_api_key.as_deref())?;
    let endpoint = chat_completions_endpoint(base_url);
    let agent = network_agent(network, timeout)?;
    let request = agent
        .post(&endpoint)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    let request = if let Some(api_key) = api_key.as_deref() {
        request.header("Authorization", &format!("Bearer {api_key}"))
    } else {
        request
    };
    let body = ChatRequest {
        model,
        messages: [
            ChatMessage {
                role: "system",
                content: system,
            },
            ChatMessage {
                role: "user",
                content: user,
            },
        ],
    };
    let mut response = request
        .send_json(body)
        .map_err(|error| Error::Message(format!("AI request failed: {error}")))?;
    let response = response
        .body_mut()
        .with_config()
        .limit(MAX_AI_RESPONSE_BYTES)
        .read_json::<ChatResponse>()
        .map_err(|error| Error::Message(format!("AI returned invalid JSON: {error}")))?;
    let content = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| Error::Message("AI response contained no choices".to_owned()))?
        .message
        .content;
    response_content_text(&content)
}

fn response_content_text(content: &serde_json::Value) -> Result<String> {
    if let Some(text) = content.as_str() {
        return Ok(text.to_owned());
    }
    let Some(parts) = content.as_array() else {
        return Err(Error::Message(
            "AI response content was not text".to_owned(),
        ));
    };
    let text = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
        .collect::<String>();
    if text.is_empty() {
        return Err(Error::Message(
            "AI response content was not text".to_owned(),
        ));
    }
    Ok(text)
}

fn extract_json_object(text: &str) -> Result<&str> {
    let start = text
        .find('{')
        .ok_or_else(|| Error::Message("AI response did not contain a JSON object".to_owned()))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| Error::Message("AI response did not contain a JSON object".to_owned()))?;
    if start > end {
        return Err(Error::Message(
            "AI response did not contain a JSON object".to_owned(),
        ));
    }
    Ok(&text[start..=end])
}

fn chat_completions_endpoint(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/chat/completions") {
        base_url.to_owned()
    } else {
        format!("{base_url}/chat/completions")
    }
}

fn models_endpoint(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    let base_url = base_url
        .strip_suffix("/chat/completions")
        .unwrap_or(base_url);
    if base_url.ends_with("/models") {
        base_url.to_owned()
    } else {
        format!("{base_url}/models")
    }
}

fn network_agent(network: &NetworkSettings, timeout: Duration) -> Result<ureq::Agent> {
    network.validate()?;
    let builder = ureq::Agent::config_builder().timeout_global(Some(timeout));
    let builder = match network.proxy_mode {
        ProxyMode::Environment => builder,
        ProxyMode::Explicit => builder.proxy(network.explicit_proxy()?),
        ProxyMode::Direct => builder.proxy(None),
    };
    Ok(builder.build().into())
}

fn latest_release_assets(
    network: &NetworkSettings,
    github_api_key: Option<&str>,
    repository: &str,
) -> Result<Vec<String>> {
    let agent = network_agent(network, REQUEST_TIMEOUT)?;
    let url = format!("https://api.github.com/repos/{repository}/releases/latest");
    let request = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    let request = if let Some(api_key) = github_api_key {
        request.header("Authorization", &format!("Bearer {api_key}"))
    } else {
        request
    };
    let mut response = request
        .call()
        .map_err(|error| Error::Message(format!("GitHub release lookup failed: {error}")))?;
    let release = response
        .body_mut()
        .with_config()
        .limit(MAX_GITHUB_RELEASE_BYTES)
        .read_json::<GithubLatestRelease>()
        .map_err(|error| {
            Error::Message(format!("GitHub returned invalid release data: {error}"))
        })?;
    Ok(release.assets.into_iter().map(|asset| asset.name).collect())
}

fn normalize_github_repository(value: &str) -> Result<String> {
    let mut value = value.trim().trim_end_matches('/');
    if let Some(repository) = value.strip_prefix("https://github.com/") {
        value = repository;
    } else if let Some(repository) = value.strip_prefix("http://github.com/") {
        value = repository;
    }
    value = value.strip_suffix(".git").unwrap_or(value);
    let parts = value.split('/').collect::<Vec<_>>();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
        })
    {
        return Err(Error::Message(
            "Enter a GitHub repository as owner/repo or a github.com URL before using AI"
                .to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn program_on_path(program: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    let extensions: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    env::split_paths(&path).any(|directory| {
        extensions.iter().any(|extension| {
            let candidate = if extension.is_empty() {
                directory.join(program)
            } else {
                directory.join(format!("{program}{extension}"))
            };
            candidate.is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LatestVersionSource, ReleaseUpdatePolicy, ToolBackground};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        thread,
    };

    fn spawn_chat_server(
        response_content: &str,
        expected_authorization: Option<&str>,
    ) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let address = listener.local_addr().expect("listener address");
        let response_content = response_content.to_owned();
        let expected_authorization = expected_authorization.map(str::to_owned);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("AI request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            assert!(
                request_line.starts_with("POST /v1/chat/completions "),
                "request: {request_line}"
            );
            let mut content_length = 0;
            let mut authorization = None;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("request header");
                if header == "\r\n" {
                    break;
                }
                let lower = header.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    content_length = value.trim().parse::<usize>().expect("content length");
                }
                if lower.starts_with("authorization:") {
                    authorization = header
                        .split_once(':')
                        .map(|(_, value)| value.trim().to_owned());
                }
            }
            assert_eq!(authorization, expected_authorization);
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).expect("request body");
            let body: serde_json::Value = serde_json::from_slice(&body).expect("request JSON");
            assert_eq!(body["model"], "test-model");
            assert_eq!(body["messages"][0]["role"], "system");
            assert_eq!(body["messages"][1]["role"], "user");

            let response = serde_json::json!({
                "choices": [{
                    "message": {
                        "content": response_content
                    }
                }]
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .expect("AI response");
        });
        (address, server)
    }

    fn spawn_models_server(
        model_ids: &[&str],
        expected_authorization: Option<&str>,
    ) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let address = listener.local_addr().expect("listener address");
        let model_ids = model_ids
            .iter()
            .map(|id| (*id).to_owned())
            .collect::<Vec<_>>();
        let expected_authorization = expected_authorization.map(str::to_owned);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("AI models request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            assert!(
                request_line.starts_with("GET /v1/models "),
                "request: {request_line}"
            );
            let mut authorization = None;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("request header");
                if header == "\r\n" {
                    break;
                }
                if header.to_ascii_lowercase().starts_with("authorization:") {
                    authorization = header
                        .split_once(':')
                        .map(|(_, value)| value.trim().to_owned());
                }
            }
            assert_eq!(authorization, expected_authorization);

            let response = serde_json::json!({
                "object": "list",
                "data": model_ids
                    .into_iter()
                    .map(|id| serde_json::json!({ "id": id, "object": "model" }))
                    .collect::<Vec<_>>(),
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .expect("AI models response");
        });
        (address, server)
    }

    #[test]
    fn appends_chat_completions_once() {
        assert_eq!(
            chat_completions_endpoint("https://example.com/v1"),
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_endpoint("https://example.com/v1/chat/completions/"),
            "https://example.com/v1/chat/completions"
        );
    }

    #[test]
    fn lists_models_before_selection_with_the_configured_api_key() {
        let token = "sk-model-list-test";
        let (address, server) = spawn_models_server(
            &["gpt-4.1", "", "gpt-4.1-mini", "gpt-4.1"],
            Some("Bearer sk-model-list-test"),
        );
        let settings = AiSettings {
            enabled: false,
            base_url: Some(format!("http://{address}/v1")),
            model: None,
            encrypted_api_key: Some(
                credential::encrypt_ai_api_key(token).expect("encrypt test API key"),
            ),
        };
        let network = NetworkSettings {
            proxy_mode: ProxyMode::Direct,
            proxy_url: None,
            no_proxy: Vec::new(),
        };

        let models = list_models(&settings, &network).expect("model list");

        assert_eq!(models, vec!["gpt-4.1", "gpt-4.1-mini"]);
        server.join().expect("test server");
    }

    #[test]
    fn extracts_json_from_plain_or_fenced_responses() {
        assert_eq!(
            extract_json_object(r#"{"name":"tool"}"#).unwrap(),
            r#"{"name":"tool"}"#
        );
        assert_eq!(
            extract_json_object("```json\n{\"name\":\"tool\"}\n```").unwrap(),
            r#"{"name":"tool"}"#
        );
    }

    #[test]
    fn normalizes_supported_github_repository_inputs() {
        assert_eq!(
            normalize_github_repository("https://github.com/owner/repo.git").unwrap(),
            "owner/repo"
        );
        assert!(normalize_github_repository("repo").is_err());
    }

    #[test]
    fn generated_monitor_must_stay_under_root_and_match_one_compatible_asset() {
        let root = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\Users\test\dvup\installs")
        } else {
            std::path::PathBuf::from("/home/test/dvup/installs")
        };
        let context = SystemContext {
            os: "linux",
            arch: "x86_64",
            available_package_managers: Vec::new(),
            install_root: root.display().to_string(),
        };
        let monitor = GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/example".to_owned(),
            asset_regex: r"^example-v[0-9.]+-linux-x86_64\.zip$".to_owned(),
            target_directory: root.join("example"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        };
        let assets = vec![
            "example-v1.2.3-linux-x86_64.zip".to_owned(),
            "example-v1.2.3-linux-aarch64.zip".to_owned(),
            "example-v1.2.3-darwin-arm64.tar.gz".to_owned(),
        ];

        assert!(
            validate_generated_monitor(monitor.clone(), "owner/example", &context, &assets).is_ok()
        );

        for generated_name in ["owner/example", "Example CLI", "示例工具"] {
            let mut invalid_name = monitor.clone();
            invalid_name.name = generated_name.to_owned();
            let normalized =
                validate_generated_monitor(invalid_name, "owner/example", &context, &assets)
                    .expect("invalid generated names are normalized");
            assert_eq!(normalized.name, "example");
        }

        let mut outside = monitor.clone();
        outside.target_directory = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\Windows\example")
        } else {
            std::path::PathBuf::from("/opt/example")
        };
        assert!(validate_generated_monitor(outside, "owner/example", &context, &assets).is_err());

        let mut root_target = monitor.clone();
        root_target.target_directory = root.clone();
        assert!(
            validate_generated_monitor(root_target, "owner/example", &context, &assets).is_err()
        );

        let mut traversal = monitor.clone();
        traversal.target_directory = root.join("nested").join("..").join("..").join("outside");
        assert!(validate_generated_monitor(traversal, "owner/example", &context, &assets).is_err());

        let mut automatic = monitor.clone();
        automatic.update_policy = ReleaseUpdatePolicy::Automatic;
        assert!(validate_generated_monitor(automatic, "owner/example", &context, &assets).is_err());

        let mut disabled = monitor.clone();
        disabled.enabled = false;
        assert!(validate_generated_monitor(disabled, "owner/example", &context, &assets).is_err());

        let mut oversized = monitor.clone();
        oversized.name = "x".repeat(MAX_GENERATED_NAME_BYTES + 1);
        assert!(validate_generated_monitor(oversized, "owner/example", &context, &assets).is_err());

        let mut broad = monitor.clone();
        broad.asset_regex = r"^example-v[0-9.]+-.*$".to_owned();
        assert!(validate_generated_monitor(broad, "owner/example", &context, &assets).is_err());

        let mut wrong_format = monitor;
        wrong_format.format = ReleaseAssetFormat::TarGz;
        assert!(
            validate_generated_monitor(wrong_format.clone(), "owner/example", &context, &assets)
                .is_err()
        );

        let mut wrong_os = wrong_format.clone();
        wrong_os.asset_regex = r"^example-v[0-9.]+-darwin-arm64\.tar\.gz$".to_owned();
        assert!(validate_generated_monitor(wrong_os, "owner/example", &context, &assets).is_err());

        let mut wrong_arch = wrong_format;
        wrong_arch.asset_regex = r"^example-v[0-9.]+-linux-aarch64\.zip$".to_owned();
        wrong_arch.format = ReleaseAssetFormat::Zip;
        assert!(
            validate_generated_monitor(wrong_arch, "owner/example", &context, &assets).is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn generated_dmg_monitors_default_to_the_applications_directory() {
        let install_root = std::path::PathBuf::from("/Users/test/.local/share/dvup/installs");
        let context = SystemContext {
            os: "macos",
            arch: "aarch64",
            available_package_managers: Vec::new(),
            install_root: install_root.display().to_string(),
        };
        let monitor = GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/example".to_owned(),
            asset_regex: r"^Example-[0-9.]+-arm64\.dmg$".to_owned(),
            target_directory: install_root.join("Example.app"),
            format: ReleaseAssetFormat::Dmg,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        };
        let assets = vec!["Example-1.2.3-arm64.dmg".to_owned()];

        let generated = validate_generated_monitor(monitor, "owner/example", &context, &assets)
            .expect("generated DMG monitor");

        assert_eq!(
            generated.target_directory,
            std::path::PathBuf::from("/Applications/Example.app")
        );
    }

    #[test]
    fn compatible_chat_completion_response_generates_a_command() {
        let (address, server) = spawn_chat_server(
            r#"{
                "name":"ripgrep",
                "tool": {
                    "update":["brew","upgrade","ripgrep"],
                    "probe":["rg","--version"],
                    "latest":{"provider":"github_release","repository":"BurntSushi/ripgrep"},
                    "update_version":["cargo","install","ripgrep","--version","{version}"],
                    "background":"always",
                    "wait_for":["rg"],
                    "processes":[{"name":"rg","command_contains":"--files","action":"fail","terminate_grace_secs":3}],
                    "lock_timeout_secs":45,
                    "retries":3,
                    "retry_delay_secs":2,
                    "platforms":["macos"],
                    "resource_group":"homebrew"
                }
            }"#,
            None,
        );
        let settings = AiSettings {
            enabled: true,
            base_url: Some(format!("http://{address}/v1")),
            model: Some("test-model".to_owned()),
            encrypted_api_key: None,
        };
        let network = NetworkSettings {
            proxy_mode: ProxyMode::Direct,
            proxy_url: None,
            no_proxy: Vec::new(),
        };
        let context = SystemContext {
            os: "macos",
            arch: "aarch64",
            available_package_managers: vec!["brew"],
            install_root: "/tmp/dvup/installs".to_owned(),
        };

        let generated =
            generate_command(&settings, &network, &context, "ripgrep", "").expect("generated");

        assert_eq!(generated.name, "ripgrep");
        assert_eq!(generated.tool.update, ["brew", "upgrade", "ripgrep"]);
        assert_eq!(generated.tool.probe, ["rg", "--version"]);
        assert!(matches!(
            generated.tool.latest,
            Some(LatestVersionSource::GithubRelease { ref repository })
                if repository == "BurntSushi/ripgrep"
        ));
        assert_eq!(
            generated
                .tool
                .update_version
                .as_deref()
                .map(|parts| parts.iter().map(String::as_str).collect::<Vec<_>>()),
            Some(vec![
                "cargo",
                "install",
                "ripgrep",
                "--version",
                "{version}"
            ])
        );
        assert_eq!(generated.tool.background, ToolBackground::Always);
        assert_eq!(
            generated
                .tool
                .wait_for
                .as_deref()
                .map(|parts| parts.iter().map(String::as_str).collect::<Vec<_>>()),
            Some(vec!["rg"])
        );
        assert_eq!(generated.tool.processes.len(), 1);
        assert_eq!(generated.tool.processes[0].name, "rg");
        assert_eq!(generated.tool.lock_timeout_secs, 45);
        assert_eq!(generated.tool.retries, 3);
        assert_eq!(generated.tool.retry_delay_secs, 2);
        assert_eq!(generated.tool.platforms, ["macos"]);
        assert_eq!(generated.tool.resource_group.as_deref(), Some("homebrew"));
        server.join().expect("test server");
    }

    #[test]
    fn generated_command_requires_every_user_tool_field() {
        let (address, server) = spawn_chat_server(
            r#"{
                "name":"ripgrep",
                "tool": {
                    "update":["brew","upgrade","ripgrep"],
                    "probe":["rg","--version"]
                }
            }"#,
            None,
        );
        let settings = AiSettings {
            enabled: true,
            base_url: Some(format!("http://{address}/v1")),
            model: Some("test-model".to_owned()),
            encrypted_api_key: None,
        };
        let network = NetworkSettings {
            proxy_mode: ProxyMode::Direct,
            proxy_url: None,
            no_proxy: Vec::new(),
        };
        let context = SystemContext {
            os: "macos",
            arch: "aarch64",
            available_package_managers: vec!["brew"],
            install_root: "/tmp/dvup/installs".to_owned(),
        };

        let error = generate_command(&settings, &network, &context, "ripgrep", "")
            .expect_err("incomplete AI tool configuration must be rejected");

        assert!(
            error.to_string().contains("missing field"),
            "error: {error}"
        );
        server.join().expect("test server");
    }

    #[test]
    fn connection_test_checks_the_model_and_encrypted_api_key() {
        let token = "sk-connection-test";
        let (address, server) = spawn_chat_server("OK", Some("Bearer sk-connection-test"));
        let settings = AiSettings {
            enabled: true,
            base_url: Some(format!("http://{address}/v1")),
            model: Some("test-model".to_owned()),
            encrypted_api_key: Some(
                credential::encrypt_ai_api_key(token).expect("encrypt test API key"),
            ),
        };
        let network = NetworkSettings {
            proxy_mode: ProxyMode::Direct,
            proxy_url: None,
            no_proxy: Vec::new(),
        };

        test_connection(&settings, &network).expect("connection test");

        server.join().expect("test server");
    }
}
