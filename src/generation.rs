use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::PathBuf,
    process::Command,
};

use serde::Deserialize;

use crate::{
    config::{LatestVersionSource, PackageManager, valid_github_release_monitor_name},
    error::{Error, Result},
    release::normalize_github_repository,
    settings::NetworkSettings,
    version,
};

const HOMEBREW_FORMULA_API: &str = "https://formulae.brew.sh/api/formula";
const MAX_SOURCE_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_NPM_METADATA_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PackageMetadata {
    pub(crate) latest_version: String,
    pub(crate) executables: Vec<String>,
}

/// Resolves one package only through its fixed official registry and safe local queries.
pub(crate) fn resolve_package_metadata(
    manager: PackageManager,
    package: &str,
    network: &NetworkSettings,
) -> Result<PackageMetadata> {
    validate_package(package)?;
    let agent = version::network_agent(network)?;
    let (latest_version, official_executables) = match manager {
        PackageManager::Npm | PackageManager::Pnpm => fetch_npm_package_metadata(package, &agent)?,
        _ => {
            let source = manager.latest_source(package);
            let latest_version = version::fetch_latest(&source, &agent, None)
                .map_err(|error| Error::Message(error.detail().to_owned()))?;
            (latest_version, Vec::new())
        }
    };
    let mut executables = official_executables.into_iter().collect::<BTreeSet<_>>();
    executables.extend(discover_installed_executables(manager, package));
    Ok(PackageMetadata {
        latest_version,
        executables: executables.into_iter().collect(),
    })
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NpmBin {
    Single(String),
    Multiple(BTreeMap<String, String>),
}

#[derive(Deserialize)]
struct NpmPackageMetadata {
    version: String,
    #[serde(default)]
    bin: Option<NpmBin>,
}

fn fetch_npm_package_metadata(package: &str, agent: &ureq::Agent) -> Result<(String, Vec<String>)> {
    let url = format!(
        "https://registry.npmjs.org/{}/latest",
        version::encode_path_segment(package)
    );
    let mut response = agent
        .get(&url)
        .header("User-Agent", concat!("dvup/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/json")
        .call()
        .map_err(|error| Error::Message(format!("npm Registry lookup failed: {error}")))?;
    let metadata = response
        .body_mut()
        .with_config()
        .limit(MAX_NPM_METADATA_BYTES)
        .read_json::<NpmPackageMetadata>()
        .map_err(|error| {
            Error::Message(format!("npm returned invalid package metadata: {error}"))
        })?;
    let latest_version = SourceFacts {
        latest_version: metadata.version,
    }
    .validated_version()?;
    let executables = match metadata.bin {
        None => Vec::new(),
        Some(NpmBin::Multiple(entries)) => entries.into_keys().collect(),
        Some(NpmBin::Single(path)) => {
            let _ = path;
            vec![default_package_executable(package)]
        }
    };
    Ok((latest_version, executables))
}

fn discover_installed_executables(manager: PackageManager, package: &str) -> Vec<String> {
    let output = match manager {
        PackageManager::Homebrew => manager_query("brew", &["list", "--formula", package]),
        PackageManager::Cargo => manager_query("cargo", &["install", "--list"]),
        PackageManager::Pipx => manager_query("pipx", &["list", "--json"]),
        PackageManager::Uv => manager_query("uv", &["tool", "list"]),
        PackageManager::Npm | PackageManager::Pnpm => None,
    };
    let Some(output) = output else {
        return Vec::new();
    };
    let candidates = match manager {
        PackageManager::Homebrew => homebrew_executables(&output),
        PackageManager::Cargo => indented_package_executables(&output, package),
        PackageManager::Pipx => pipx_executables(&output, package),
        PackageManager::Uv => indented_package_executables(&output, package),
        PackageManager::Npm | PackageManager::Pnpm => Vec::new(),
    };
    candidates
        .into_iter()
        .filter(|candidate| valid_executable_candidate(candidate))
        .collect()
}

fn manager_query(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn homebrew_executables(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let path = PathBuf::from(line.trim());
            let parent = path.parent()?.file_name()?.to_str()?;
            (parent == "bin").then(|| path.file_name()?.to_str().map(str::to_owned))?
        })
        .collect()
}

fn indented_package_executables(output: &str, package: &str) -> Vec<String> {
    let mut in_package = false;
    let mut executables = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if in_package
            && (line.chars().next().is_some_and(char::is_whitespace)
                || trimmed.starts_with(['-', '*']))
        {
            let candidate = trimmed
                .trim_start_matches(['-', '*'])
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches(':');
            if !candidate.is_empty() {
                executables.push(candidate.to_owned());
            }
            continue;
        }
        if !line.chars().next().is_some_and(char::is_whitespace) {
            let header = line.trim();
            in_package = header == package
                || header
                    .strip_prefix(package)
                    .is_some_and(|suffix| suffix.starts_with(" v"));
        }
    }
    executables
}

fn pipx_executables(output: &str, package: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    let Some(venv) = value.get("venvs").and_then(|venvs| venvs.get(package)) else {
        return Vec::new();
    };
    let mut executables = Vec::new();
    collect_apps(venv, &mut executables);
    executables
}

fn collect_apps(value: &serde_json::Value, executables: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(entries) => {
            if let Some(apps) = entries.get("apps").and_then(serde_json::Value::as_array) {
                executables.extend(
                    apps.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned),
                );
            }
            for child in entries.values() {
                collect_apps(child, executables);
            }
        }
        serde_json::Value::Array(entries) => {
            for child in entries {
                collect_apps(child, executables);
            }
        }
        _ => {}
    }
}

fn default_package_executable(package: &str) -> String {
    package
        .rsplit('/')
        .next()
        .unwrap_or(package)
        .trim_start_matches('@')
        .to_owned()
}

fn valid_executable_candidate(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.trim() == candidate
        && candidate
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.+".contains(character))
}

/// A command identity proposed by AI. Runtime behavior is compiled locally.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandCandidate {
    pub(crate) name: String,
    pub(crate) manager: PackageManager,
    pub(crate) package: String,
}

impl CommandCandidate {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_candidate_name(&self.name, "command candidate")?;
        validate_package(&self.package)?;
        Ok(())
    }
}

/// A GitHub repository identity proposed by AI. Assets are selected locally.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubCandidate {
    pub(crate) name: String,
    pub(crate) repository: String,
}

impl GithubCandidate {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_candidate_name(&self.name, "GitHub candidate")?;
        let normalized = normalize_github_repository(&self.repository)?;
        if normalized != self.repository {
            return Err(Error::Message(
                "AI GitHub candidate repository must use canonical owner/name form".to_owned(),
            ));
        }
        Ok(())
    }
}

fn validate_candidate_name(name: &str, label: &str) -> Result<()> {
    if !valid_github_release_monitor_name(name) {
        return Err(Error::Message(format!(
            "{label} name must use only letters, digits, dash, underscore, or dot"
        )));
    }
    Ok(())
}

fn validate_package(package: &str) -> Result<()> {
    if package.is_empty()
        || package.trim() != package
        || package.chars().any(char::is_whitespace)
        || package.chars().any(char::is_control)
    {
        return Err(Error::Message(
            "AI command candidate package must be non-empty without padding or control characters"
                .to_owned(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedCommandCandidate {
    pub(crate) name: String,
    pub(crate) manager: PackageManager,
    pub(crate) package: String,
    pub(crate) latest_version: String,
    pub(crate) collected_at_unix_secs: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedGithubCandidate {
    pub(crate) name: String,
    pub(crate) repository: String,
    pub(crate) latest_version: String,
    pub(crate) collected_at_unix_secs: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedCommandCandidate {
    pub(crate) name: String,
    pub(crate) manager: PackageManager,
    pub(crate) package: String,
    pub(crate) error: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedGithubCandidate {
    pub(crate) name: String,
    pub(crate) repository: String,
    pub(crate) error: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandCandidateVerification {
    pub(crate) verified: Vec<VerifiedCommandCandidate>,
    pub(crate) rejected: Vec<RejectedCommandCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GithubCandidateVerification {
    pub(crate) verified: Vec<VerifiedGithubCandidate>,
    pub(crate) rejected: Vec<RejectedGithubCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceFacts {
    pub(crate) latest_version: String,
}

impl SourceFacts {
    fn validated_version(self) -> Result<String> {
        if self.latest_version.is_empty() || self.latest_version.trim() != self.latest_version {
            return Err(Error::Message(
                "authoritative source returned an invalid latest version".to_owned(),
            ));
        }
        Ok(self.latest_version)
    }
}

pub(crate) trait AuthoritativeSources {
    fn homebrew_formula(&self, formula: &str) -> Result<SourceFacts>;
    fn npm_package(&self, package: &str) -> Result<SourceFacts>;
    fn pypi_project(&self, package: &str) -> Result<SourceFacts>;
    fn crate_package(&self, package: &str) -> Result<SourceFacts>;
    fn github_release(&self, repository: &str) -> Result<SourceFacts>;
}

pub(crate) struct LiveAuthoritativeSources<'a> {
    agent: ureq::Agent,
    github_api_key: Option<&'a str>,
    homebrew_base_url: String,
}

impl<'a> LiveAuthoritativeSources<'a> {
    pub(crate) fn new(agent: ureq::Agent, github_api_key: Option<&'a str>) -> Self {
        Self {
            agent,
            github_api_key,
            homebrew_base_url: HOMEBREW_FORMULA_API.to_owned(),
        }
    }

    #[cfg(test)]
    fn with_homebrew_base_url(
        agent: ureq::Agent,
        github_api_key: Option<&'a str>,
        homebrew_base_url: String,
    ) -> Self {
        Self {
            agent,
            github_api_key,
            homebrew_base_url,
        }
    }

    fn registry_facts(&self, source: LatestVersionSource, label: &str) -> Result<SourceFacts> {
        let latest_version = version::fetch_latest(&source, &self.agent, self.github_api_key)
            .map_err(|error| {
                Error::Message(format!(
                    "{label} authoritative lookup failed: {}",
                    error.detail()
                ))
            })?;
        Ok(SourceFacts { latest_version })
    }
}

#[derive(Deserialize)]
struct HomebrewFormula {
    name: String,
    versions: HomebrewVersions,
    deprecated: bool,
    disabled: bool,
}

#[derive(Deserialize)]
struct HomebrewVersions {
    stable: String,
}

impl AuthoritativeSources for LiveAuthoritativeSources<'_> {
    fn homebrew_formula(&self, formula: &str) -> Result<SourceFacts> {
        let url = format!(
            "{}/{}.json",
            self.homebrew_base_url,
            version::encode_path_segment(formula)
        );
        let mut response = self
            .agent
            .get(&url)
            .header("User-Agent", concat!("dvup/", env!("CARGO_PKG_VERSION")))
            .header("Accept", "application/json")
            .call()
            .map_err(|error| Error::Message(format!("Homebrew formula lookup failed: {error}")))?;
        let metadata = response
            .body_mut()
            .with_config()
            .limit(MAX_SOURCE_RESPONSE_BYTES)
            .read_json::<HomebrewFormula>()
            .map_err(|error| {
                Error::Message(format!(
                    "Homebrew returned invalid formula metadata: {error}"
                ))
            })?;
        if metadata.name != formula {
            return Err(Error::Message(format!(
                "Homebrew returned formula `{}` instead of locked formula `{formula}`",
                metadata.name
            )));
        }
        if metadata.disabled || metadata.deprecated {
            return Err(Error::Message(format!(
                "Homebrew formula `{formula}` is disabled or deprecated"
            )));
        }
        let facts = SourceFacts {
            latest_version: metadata.versions.stable,
        };
        facts.clone().validated_version()?;
        Ok(facts)
    }

    fn npm_package(&self, package: &str) -> Result<SourceFacts> {
        self.registry_facts(
            LatestVersionSource::Npm {
                package: package.to_owned(),
            },
            "npm",
        )
    }

    fn pypi_project(&self, package: &str) -> Result<SourceFacts> {
        self.registry_facts(
            LatestVersionSource::Pypi {
                package: package.to_owned(),
            },
            "PyPI",
        )
    }

    fn crate_package(&self, package: &str) -> Result<SourceFacts> {
        self.registry_facts(
            LatestVersionSource::CratesIo {
                package: package.to_owned(),
            },
            "crates.io",
        )
    }

    fn github_release(&self, repository: &str) -> Result<SourceFacts> {
        self.registry_facts(
            LatestVersionSource::GithubRelease {
                repository: repository.to_owned(),
            },
            "GitHub Release",
        )
    }
}

pub(crate) fn verify_live_command_candidates(
    candidates: Vec<CommandCandidate>,
    network: &NetworkSettings,
    github_api_key: Option<&str>,
    collected_at_unix_secs: u64,
) -> Result<CommandCandidateVerification> {
    let sources = LiveAuthoritativeSources::new(version::network_agent(network)?, github_api_key);
    Ok(verify_command_candidates(
        candidates,
        &sources,
        collected_at_unix_secs,
    ))
}

pub(crate) fn verify_live_github_candidates(
    candidates: Vec<GithubCandidate>,
    network: &NetworkSettings,
    github_api_key: Option<&str>,
    collected_at_unix_secs: u64,
) -> Result<GithubCandidateVerification> {
    let sources = LiveAuthoritativeSources::new(version::network_agent(network)?, github_api_key);
    Ok(verify_github_candidates(
        candidates,
        &sources,
        collected_at_unix_secs,
    ))
}

pub(crate) fn verify_command_candidates(
    candidates: Vec<CommandCandidate>,
    sources: &impl AuthoritativeSources,
    collected_at_unix_secs: u64,
) -> CommandCandidateVerification {
    let mut verified = Vec::new();
    let mut rejected = Vec::new();
    for candidate in candidates {
        let result = candidate.validate().and_then(|()| {
            let facts = match candidate.manager {
                PackageManager::Homebrew => sources.homebrew_formula(&candidate.package),
                PackageManager::Npm | PackageManager::Pnpm => {
                    sources.npm_package(&candidate.package)
                }
                PackageManager::Cargo => sources.crate_package(&candidate.package),
                PackageManager::Pipx | PackageManager::Uv => {
                    sources.pypi_project(&candidate.package)
                }
            }?;
            facts.validated_version()
        });
        match result {
            Ok(latest_version) => verified.push(VerifiedCommandCandidate {
                name: candidate.name,
                manager: candidate.manager,
                package: candidate.package,
                latest_version,
                collected_at_unix_secs,
            }),
            Err(error) => rejected.push(RejectedCommandCandidate {
                name: candidate.name,
                manager: candidate.manager,
                package: candidate.package,
                error: error.to_string(),
            }),
        }
    }
    CommandCandidateVerification { verified, rejected }
}

pub(crate) fn verify_github_candidates(
    candidates: Vec<GithubCandidate>,
    sources: &impl AuthoritativeSources,
    collected_at_unix_secs: u64,
) -> GithubCandidateVerification {
    let mut verified = Vec::new();
    let mut rejected = Vec::new();
    for candidate in candidates {
        let result = candidate.validate().and_then(|()| {
            sources
                .github_release(&candidate.repository)?
                .validated_version()
        });
        match result {
            Ok(latest_version) => verified.push(VerifiedGithubCandidate {
                name: candidate.name,
                repository: candidate.repository,
                latest_version,
                collected_at_unix_secs,
            }),
            Err(error) => rejected.push(RejectedGithubCandidate {
                name: candidate.name,
                repository: candidate.repository,
                error: error.to_string(),
            }),
        }
    }
    GithubCandidateVerification { verified, rejected }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, io::Write, net::TcpListener, thread};

    use super::*;

    #[derive(Default)]
    struct FakeSources {
        versions: HashMap<String, String>,
    }

    impl FakeSources {
        fn facts(&self, key: &str) -> Result<SourceFacts> {
            self.versions
                .get(key)
                .cloned()
                .map(|latest_version| SourceFacts { latest_version })
                .ok_or_else(|| Error::Message(format!("missing {key}")))
        }
    }

    impl AuthoritativeSources for FakeSources {
        fn homebrew_formula(&self, formula: &str) -> Result<SourceFacts> {
            self.facts(&format!("brew:{formula}"))
        }

        fn npm_package(&self, package: &str) -> Result<SourceFacts> {
            self.facts(&format!("npm:{package}"))
        }

        fn pypi_project(&self, package: &str) -> Result<SourceFacts> {
            self.facts(&format!("pypi:{package}"))
        }

        fn crate_package(&self, package: &str) -> Result<SourceFacts> {
            self.facts(&format!("crate:{package}"))
        }

        fn github_release(&self, repository: &str) -> Result<SourceFacts> {
            self.facts(&format!("github:{repository}"))
        }
    }

    #[test]
    fn command_candidates_only_use_their_managers_official_registry() {
        let sources = FakeSources {
            versions: HashMap::from([
                ("npm:@scope/tool".to_owned(), "2.0.0".to_owned()),
                ("crate:ripgrep".to_owned(), "14.1.1".to_owned()),
            ]),
        };
        let verification = verify_command_candidates(
            vec![
                CommandCandidate {
                    name: "tool".to_owned(),
                    manager: PackageManager::Pnpm,
                    package: "@scope/tool".to_owned(),
                },
                CommandCandidate {
                    name: "ripgrep".to_owned(),
                    manager: PackageManager::Cargo,
                    package: "ripgrep".to_owned(),
                },
            ],
            &sources,
            42,
        );

        assert!(verification.rejected.is_empty());
        assert_eq!(verification.verified[0].latest_version, "2.0.0");
        assert_eq!(verification.verified[1].latest_version, "14.1.1");
        assert_eq!(verification.verified[0].collected_at_unix_secs, 42);
    }

    #[test]
    fn command_and_github_verification_results_are_structurally_separate() {
        let sources = FakeSources {
            versions: HashMap::from([(
                "github:BurntSushi/ripgrep".to_owned(),
                "14.1.1".to_owned(),
            )]),
        };
        let verification = verify_github_candidates(
            vec![GithubCandidate {
                name: "ripgrep".to_owned(),
                repository: "BurntSushi/ripgrep".to_owned(),
            }],
            &sources,
            7,
        );

        assert!(verification.rejected.is_empty());
        assert_eq!(verification.verified[0].repository, "BurntSushi/ripgrep");
        assert_eq!(verification.verified[0].latest_version, "14.1.1");
    }

    #[test]
    fn invalid_identity_is_rejected_without_registry_fallback() {
        let sources = FakeSources::default();
        let verification = verify_command_candidates(
            vec![CommandCandidate {
                name: "invalid name".to_owned(),
                manager: PackageManager::Npm,
                package: "tool".to_owned(),
            }],
            &sources,
            1,
        );

        assert!(verification.verified.is_empty());
        assert_eq!(verification.rejected.len(), 1);
        assert!(
            verification.rejected[0]
                .error
                .contains("name must use only")
        );
    }

    #[test]
    fn homebrew_lookup_rejects_mismatched_formula_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let body = r#"{"name":"other","versions":{"stable":"1.0.0"},"deprecated":false,"disabled":false}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        let sources = LiveAuthoritativeSources::with_homebrew_base_url(
            ureq::Agent::new_with_defaults(),
            None,
            format!("http://{address}"),
        );

        let error = sources
            .homebrew_formula("expected")
            .expect_err("mismatched metadata must fail");
        server.join().expect("join server");
        assert!(error.to_string().contains("instead of locked formula"));
    }

    #[test]
    fn package_metadata_parsers_return_real_executable_names() {
        assert_eq!(
            homebrew_executables(
                "/opt/homebrew/Cellar/example/1.0/bin/example\n/opt/homebrew/Cellar/example/1.0/share/doc.txt\n"
            ),
            ["example"]
        );
        assert_eq!(
            indented_package_executables(
                "other v1.0.0:\n    other\nexample v2.0.0:\n    example\n    example-admin\n",
                "example"
            ),
            ["example", "example-admin"]
        );
        assert_eq!(
            indented_package_executables("example v2.0.0\n- example\n", "example"),
            ["example"]
        );
        assert_eq!(
            pipx_executables(
                r#"{"venvs":{"example":{"metadata":{"main_package":{"apps":["example","example-admin"]}}}}}"#,
                "example"
            ),
            ["example", "example-admin"]
        );
    }

    #[test]
    fn npm_metadata_supports_single_and_multiple_bin_shapes() {
        let single: NpmPackageMetadata =
            serde_json::from_str(r#"{"version":"1.2.3","bin":"cli.js"}"#)
                .expect("single bin metadata");
        let multiple: NpmPackageMetadata =
            serde_json::from_str(r#"{"version":"1.2.3","bin":{"one":"one.js","two":"two.js"}}"#)
                .expect("multiple bin metadata");

        assert!(matches!(single.bin, Some(NpmBin::Single(_))));
        assert!(matches!(multiple.bin, Some(NpmBin::Multiple(ref bins)) if bins.len() == 2));
    }
}
