use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::{
    config::LatestVersionSource,
    error::{Error, Result},
    settings::{NetworkSettings, ProxyMode},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("dvup/", env!("CARGO_PKG_VERSION"));

pub(crate) fn extract_versions(stdout: &[u8], stderr: &[u8]) -> Vec<String> {
    let mut output = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        output.push('\n');
        output.push_str(&String::from_utf8_lossy(stderr));
    }
    let plain = strip_terminal_escapes(&output);
    let mut versions = plain
        .split_whitespace()
        .filter_map(version_token)
        .collect::<Vec<_>>();
    versions.sort();
    versions.dedup();
    versions
}

pub(crate) fn version_token(token: &str) -> Option<String> {
    let candidate = token.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | '+' | '_')
    });
    let numeric = candidate
        .strip_prefix('v')
        .or_else(|| candidate.strip_prefix('V'))
        .unwrap_or(candidate);
    (numeric.contains('.')
        && numeric.chars().any(|character| character.is_ascii_digit())
        && numeric.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | '_')
        }))
    .then(|| numeric.to_owned())
}

fn strip_terminal_escapes(input: &str) -> String {
    #[derive(Clone, Copy)]
    enum Escape {
        None,
        Start,
        Csi,
        Osc,
        OscTerminator,
    }

    let mut state = Escape::None;
    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        state = match state {
            Escape::None if character == '\u{1b}' => Escape::Start,
            Escape::None => {
                if character == '\n' || character == '\t' || !character.is_control() {
                    output.push(character);
                }
                Escape::None
            }
            Escape::Start if character == '[' => Escape::Csi,
            Escape::Start if character == ']' => Escape::Osc,
            Escape::Start => Escape::None,
            Escape::Csi if ('@'..='~').contains(&character) => Escape::None,
            Escape::Csi => Escape::Csi,
            Escape::Osc if character == '\u{7}' => Escape::None,
            Escape::Osc if character == '\u{1b}' => Escape::OscTerminator,
            Escape::Osc => Escape::Osc,
            Escape::OscTerminator if character == '\\' => Escape::None,
            Escape::OscTerminator => Escape::Osc,
        };
    }
    output
}

#[derive(Deserialize)]
struct NpmRelease {
    version: String,
}

#[derive(Deserialize)]
struct HomebrewFormula {
    versions: HomebrewVersions,
}

#[derive(Deserialize)]
struct HomebrewVersions {
    stable: String,
}

#[derive(Deserialize)]
struct PypiResponse {
    info: PypiProject,
}

#[derive(Deserialize)]
struct PypiProject {
    version: String,
}

#[derive(Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    package: CrateRelease,
}

#[derive(Deserialize)]
struct CrateRelease {
    max_version: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

#[derive(Deserialize)]
struct GithubTag {
    name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LatestVersionErrorKind {
    RateLimited,
    Authentication,
    NotFound,
    RequestFailed,
    InvalidResponse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LatestVersionError {
    pub(crate) kind: LatestVersionErrorKind,
    detail: String,
}

impl LatestVersionError {
    pub(crate) fn new(kind: LatestVersionErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    fn request(error: ureq::Error) -> Self {
        let kind = match error {
            ureq::Error::StatusCode(401) => LatestVersionErrorKind::Authentication,
            ureq::Error::StatusCode(403 | 429) => LatestVersionErrorKind::RateLimited,
            ureq::Error::StatusCode(404) => LatestVersionErrorKind::NotFound,
            _ => LatestVersionErrorKind::RequestFailed,
        };
        Self::new(kind, error.to_string())
    }

    fn invalid_response(error: impl std::fmt::Display) -> Self {
        Self::new(LatestVersionErrorKind::InvalidResponse, error.to_string())
    }

    fn not_found(detail: impl Into<String>) -> Self {
        Self::new(LatestVersionErrorKind::NotFound, detail)
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

/// Queries the authoritative source configured for a tool's latest release.
pub fn fetch_latest(
    source: &LatestVersionSource,
    agent: &ureq::Agent,
    github_api_key: Option<&str>,
) -> std::result::Result<String, LatestVersionError> {
    let raw = match source {
        LatestVersionSource::Homebrew { formula } => {
            let url = format!(
                "https://formulae.brew.sh/api/formula/{}.json",
                encode_path_segment(formula)
            );
            let mut response = latest_request(agent, &url, None)?;
            response
                .body_mut()
                .read_json::<HomebrewFormula>()
                .map_err(LatestVersionError::invalid_response)?
                .versions
                .stable
        }
        LatestVersionSource::Npm { package } => {
            let url = format!(
                "https://registry.npmjs.org/{}/latest",
                encode_path_segment(package)
            );
            let mut response = latest_request(agent, &url, None)?;
            response
                .body_mut()
                .read_json::<NpmRelease>()
                .map_err(LatestVersionError::invalid_response)?
                .version
        }
        LatestVersionSource::Pypi { package } => {
            let url = format!(
                "https://pypi.org/pypi/{}/json",
                encode_path_segment(package)
            );
            let mut response = latest_request(agent, &url, None)?;
            response
                .body_mut()
                .read_json::<PypiResponse>()
                .map_err(LatestVersionError::invalid_response)?
                .info
                .version
        }
        LatestVersionSource::CratesIo { package } => {
            let url = format!(
                "https://crates.io/api/v1/crates/{}",
                encode_path_segment(package)
            );
            let mut response = latest_request(agent, &url, None)?;
            response
                .body_mut()
                .read_json::<CratesResponse>()
                .map_err(LatestVersionError::invalid_response)?
                .package
                .max_version
        }
        LatestVersionSource::GithubRelease { repository } => {
            let url = format!("https://api.github.com/repos/{repository}/releases/latest");
            let mut response = latest_request(agent, &url, github_api_key)?;
            response
                .body_mut()
                .read_json::<GithubRelease>()
                .map_err(LatestVersionError::invalid_response)?
                .tag_name
        }
        LatestVersionSource::GithubTag { repository } => {
            let url = format!("https://api.github.com/repos/{repository}/tags?per_page=1");
            let mut response = latest_request(agent, &url, github_api_key)?;
            response
                .body_mut()
                .read_json::<Vec<GithubTag>>()
                .map_err(LatestVersionError::invalid_response)?
                .into_iter()
                .next()
                .ok_or_else(|| LatestVersionError::not_found("GitHub returned no tags"))?
                .name
        }
    };
    normalize_release(&raw).map_err(LatestVersionError::invalid_response)
}

fn latest_request(
    agent: &ureq::Agent,
    url: &str,
    github_api_key: Option<&str>,
) -> std::result::Result<ureq::http::Response<ureq::Body>, LatestVersionError> {
    let request = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json");
    let request = if let Some(api_key) = github_api_key {
        request.header("Authorization", &format!("Bearer {api_key}"))
    } else {
        request
    };
    request.call().map_err(LatestVersionError::request)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GithubRateLimit {
    pub(crate) owner: String,
    pub(crate) limit: u64,
    pub(crate) used: u64,
    pub(crate) remaining: u64,
    pub(crate) reset_unix: u64,
}

#[derive(Debug, Deserialize)]
struct GithubAuthenticatedUser {
    login: String,
}

pub(crate) fn fetch_github_rate_limit(
    agent: &ureq::Agent,
    github_api_key: &str,
) -> Result<GithubRateLimit> {
    let mut response = request(agent, "https://api.github.com/user", Some(github_api_key))?;
    let limit = github_rate_limit_header(&response, "x-ratelimit-limit")?;
    let used = github_rate_limit_header(&response, "x-ratelimit-used")?;
    let remaining = github_rate_limit_header(&response, "x-ratelimit-remaining")?;
    let reset_unix = github_rate_limit_header(&response, "x-ratelimit-reset")?;
    let owner = response
        .body_mut()
        .read_json::<GithubAuthenticatedUser>()
        .map_err(latest_error)?
        .login;
    github_rate_limit_from_parts(owner, limit, used, remaining, reset_unix)
}

fn github_rate_limit_header(
    response: &ureq::http::Response<ureq::Body>,
    name: &str,
) -> Result<u64> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| Error::Message(format!("GitHub returned an invalid {name} header")))
}

fn github_rate_limit_from_parts(
    owner: String,
    limit: u64,
    used: u64,
    remaining: u64,
    reset_unix: u64,
) -> Result<GithubRateLimit> {
    if owner.trim().is_empty() || limit == 0 || used > limit || remaining > limit {
        return Err(Error::Message(
            "GitHub returned an invalid authenticated API status".to_owned(),
        ));
    }
    Ok(GithubRateLimit {
        owner,
        limit,
        used,
        remaining,
        reset_unix,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NetworkTestResult {
    pub(crate) name: &'static str,
    pub(crate) elapsed_ms: u128,
    pub(crate) error: Option<String>,
}

pub(crate) fn test_network(network: &NetworkSettings) -> Result<Vec<NetworkTestResult>> {
    const TARGETS: &[(&str, &str)] = &[
        ("npm", "https://registry.npmjs.org/"),
        ("PyPI", "https://pypi.org/"),
        ("crates.io", "https://crates.io/api/v1/crates/serde"),
        ("GitHub", "https://api.github.com/"),
    ];
    let agent = network_agent(network)?;
    Ok(TARGETS
        .iter()
        .map(|(name, url)| {
            let started = Instant::now();
            let error = request(&agent, url, None)
                .err()
                .map(|error| error.to_string());
            NetworkTestResult {
                name,
                elapsed_ms: started.elapsed().as_millis(),
                error,
            }
        })
        .collect())
}

pub(crate) fn network_agent(network: &NetworkSettings) -> Result<ureq::Agent> {
    network.validate()?;
    let builder = ureq::Agent::config_builder().timeout_global(Some(REQUEST_TIMEOUT));
    let builder = match network.proxy_mode {
        ProxyMode::Environment => builder,
        ProxyMode::Explicit => builder.proxy(network.explicit_proxy()?),
        ProxyMode::Direct => builder.proxy(None),
    };
    Ok(builder.build().into())
}

fn request(
    agent: &ureq::Agent,
    url: &str,
    github_api_key: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let request = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json");
    let request = if let Some(api_key) = github_api_key {
        request.header("Authorization", &format!("Bearer {api_key}"))
    } else {
        request
    };
    request.call().map_err(latest_error)
}

fn latest_error(error: impl std::fmt::Display) -> Error {
    Error::Message(format!("latest-version request failed: {error}"))
}

fn normalize_release(raw: &str) -> Result<String> {
    let value = raw.trim();
    let start = value
        .char_indices()
        .find_map(|(index, character)| character.is_ascii_digit().then_some(index))
        .ok_or_else(|| {
            Error::Message(format!(
                "latest-version source returned an invalid release `{value}`"
            ))
        })?;
    Ok(value[start..].to_owned())
}

pub(crate) fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_registry_and_github_release_versions() {
        assert_eq!(
            normalize_release("0.144.1").expect("npm version"),
            "0.144.1"
        );
        assert_eq!(
            normalize_release("v2.9.2").expect("GitHub version"),
            "2.9.2"
        );
        assert_eq!(
            normalize_release("bun-v1.3.14").expect("prefixed GitHub version"),
            "1.3.14"
        );
        assert!(normalize_release("release-without-version").is_err());
    }

    #[test]
    fn extracts_versions_from_stdout_stderr_and_ansi_output() {
        assert_eq!(
            extract_versions(b"\x1b[32mtool 1.2.3\x1b[0m\n", b"runtime v4.5.6\n"),
            ["1.2.3", "4.5.6"]
        );
        assert!(extract_versions(b"tool release", b"").is_empty());
    }

    #[test]
    fn percent_encodes_scoped_registry_packages() {
        assert_eq!(encode_path_segment("@openai/codex"), "%40openai%2Fcodex");
        assert_eq!(encode_path_segment("dvup"), "dvup");
    }

    #[test]
    fn reads_the_pypi_project_version_field() {
        let response: PypiResponse =
            serde_json::from_str(r#"{"info":{"name":"hermes-agent","version":"0.18.2"}}"#)
                .expect("PyPI response");

        assert_eq!(response.info.version, "0.18.2");
    }

    #[test]
    fn validates_authenticated_github_owner_and_rate_limit() {
        let user: GithubAuthenticatedUser =
            serde_json::from_str(r#"{"login":"octocat"}"#).expect("GitHub user response");
        let status = github_rate_limit_from_parts(user.login, 5_000, 125, 4_875, 1_800_000_000)
            .expect("valid authenticated API status");

        assert_eq!(status.owner, "octocat");
        assert_eq!(status.limit, 5_000);
        assert_eq!(status.used, 125);
        assert_eq!(status.remaining, 4_875);
        assert_eq!(status.reset_unix, 1_800_000_000);
        assert!(github_rate_limit_from_parts(String::new(), 5_000, 1, 4_999, 1).is_err());
        assert!(github_rate_limit_from_parts("octocat".to_owned(), 5_000, 5_001, 0, 1).is_err());
    }

    #[test]
    fn classifies_latest_version_http_failures_for_actionable_tui_status() {
        assert_eq!(
            LatestVersionError::request(ureq::Error::StatusCode(403)).kind,
            LatestVersionErrorKind::RateLimited
        );
        assert_eq!(
            LatestVersionError::request(ureq::Error::StatusCode(429)).kind,
            LatestVersionErrorKind::RateLimited
        );
        assert_eq!(
            LatestVersionError::request(ureq::Error::StatusCode(401)).kind,
            LatestVersionErrorKind::Authentication
        );
        assert_eq!(
            LatestVersionError::request(ureq::Error::StatusCode(404)).kind,
            LatestVersionErrorKind::NotFound
        );
    }

    #[test]
    fn agent_uses_the_selected_network_policy() {
        let direct = network_agent(&NetworkSettings {
            proxy_mode: ProxyMode::Direct,
            proxy_url: None,
            no_proxy: Vec::new(),
        })
        .expect("direct agent");
        assert!(direct.config().proxy().is_none());

        let explicit = network_agent(&NetworkSettings {
            proxy_mode: ProxyMode::Explicit,
            proxy_url: Some("http://127.0.0.1:7890".to_owned()),
            no_proxy: vec!["localhost".to_owned()],
        })
        .expect("explicit agent");
        let proxy = explicit.config().proxy().expect("configured proxy");
        assert_eq!(proxy.host(), "127.0.0.1");
        assert_eq!(proxy.port(), 7890);
        assert!(proxy.is_no_proxy(&"https://localhost/".parse().expect("URI")));
    }
}
