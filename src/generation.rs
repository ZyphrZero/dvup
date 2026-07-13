use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    config::LatestVersionSource,
    error::{Error, Result},
    settings::NetworkSettings,
    version,
};

const HOMEBREW_FORMULA_API: &str = "https://formulae.brew.sh/api/formula";
const MAX_SOURCE_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateProvider {
    Homebrew,
    Npm,
    Pnpm,
    Cargo,
    Pipx,
    Uv,
}

impl UpdateProvider {
    pub(crate) const ALL: [Self; 6] = [
        Self::Homebrew,
        Self::Npm,
        Self::Pnpm,
        Self::Cargo,
        Self::Pipx,
        Self::Uv,
    ];

    pub(crate) fn label(self) -> &'static str {
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
            .position(|provider| *provider == self)
            .expect("update provider belongs to ALL");
        let next = (index as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize;
        Self::ALL[next]
    }

    pub(crate) fn update_command(self, package: &str) -> Vec<String> {
        match self {
            Self::Homebrew => strings(&["brew", "upgrade", package]),
            Self::Npm => strings(&["npm", "install", "--global", &format!("{package}@latest")]),
            Self::Pnpm => strings(&["pnpm", "add", "--global", &format!("{package}@latest")]),
            Self::Cargo => strings(&["cargo", "install", package]),
            Self::Pipx => strings(&["pipx", "upgrade", package]),
            Self::Uv => strings(&["uv", "tool", "upgrade", package]),
        }
    }

    pub(crate) fn update_version_command(self, package: &str) -> Option<Vec<String>> {
        match self {
            Self::Homebrew => None,
            Self::Npm => Some(strings(&[
                "npm",
                "install",
                "--global",
                &format!("{package}@{{version}}"),
            ])),
            Self::Pnpm => Some(strings(&[
                "pnpm",
                "add",
                "--global",
                &format!("{package}@{{version}}"),
            ])),
            Self::Cargo => Some(strings(&[
                "cargo",
                "install",
                package,
                "--version",
                "{version}",
            ])),
            Self::Pipx => Some(strings(&[
                "pipx",
                "install",
                "--force",
                &format!("{package}=={{version}}"),
            ])),
            Self::Uv => Some(strings(&[
                "uv",
                "tool",
                "install",
                "--force",
                &format!("{package}=={{version}}"),
            ])),
        }
    }

    fn evidence_provider(self) -> EvidenceProvider {
        match self {
            Self::Homebrew => EvidenceProvider::Homebrew,
            Self::Npm | Self::Pnpm => EvidenceProvider::Npm,
            Self::Cargo => EvidenceProvider::CratesIo,
            Self::Pipx | Self::Uv => EvidenceProvider::Pypi,
        }
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceProvider {
    Homebrew,
    Npm,
    Pypi,
    CratesIo,
    GithubRelease,
    GithubTag,
}

impl EvidenceProvider {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::Npm => "npm",
            Self::Pypi => "PyPI",
            Self::CratesIo => "crates.io",
            Self::GithubRelease => "GitHub Release",
            Self::GithubTag => "GitHub Tag",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct GenerationRequest {
    pub(crate) name: String,
    pub(crate) instructions: String,
    pub(crate) update_provider: UpdateProvider,
    pub(crate) update_package: String,
    pub(crate) latest: LatestVersionSource,
    pub(crate) platforms: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GenerationRequestField {
    ToolName,
    Instructions,
    UpdatePackage,
    LatestIdentifier,
}

impl GenerationRequestField {
    fn english_label(self) -> &'static str {
        match self {
            Self::ToolName => "tool name",
            Self::Instructions => "AI generation request",
            Self::UpdatePackage => "update package",
            Self::LatestIdentifier => "latest-version source identifier",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GenerationRequestIssue {
    InvalidField(GenerationRequestField),
    InvalidLatestSource,
    UnsupportedPlatform(String),
}

impl fmt::Display for GenerationRequestIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidField(field) => write!(
                formatter,
                "{} must be non-empty without surrounding whitespace",
                field.english_label()
            ),
            Self::InvalidLatestSource => {
                formatter.write_str("AI generation request GitHub repository must use owner/name")
            }
            Self::UnsupportedPlatform(platform) => write!(
                formatter,
                "AI generation request has unsupported platform `{platform}`"
            ),
        }
    }
}

impl GenerationRequest {
    fn latest_identifier(&self) -> &str {
        match &self.latest {
            LatestVersionSource::Npm { package }
            | LatestVersionSource::Pypi { package }
            | LatestVersionSource::CratesIo { package } => package,
            LatestVersionSource::GithubRelease { repository }
            | LatestVersionSource::GithubTag { repository } => repository,
        }
    }

    pub(crate) fn validation_issue(&self) -> Option<GenerationRequestIssue> {
        for (field, value) in [
            (GenerationRequestField::ToolName, self.name.as_str()),
            (
                GenerationRequestField::Instructions,
                self.instructions.as_str(),
            ),
            (
                GenerationRequestField::UpdatePackage,
                self.update_package.as_str(),
            ),
        ] {
            if value.is_empty() || value.trim() != value {
                return Some(GenerationRequestIssue::InvalidField(field));
            }
        }
        let latest_identifier = self.latest_identifier();
        if latest_identifier.is_empty() || latest_identifier.trim() != latest_identifier {
            return Some(GenerationRequestIssue::InvalidField(
                GenerationRequestField::LatestIdentifier,
            ));
        }
        if self.latest.validate("AI generation request").is_err() {
            return Some(GenerationRequestIssue::InvalidLatestSource);
        }
        for platform in &self.platforms {
            if !matches!(platform.as_str(), "windows" | "macos" | "linux") {
                return Some(GenerationRequestIssue::UnsupportedPlatform(
                    platform.clone(),
                ));
            }
        }
        None
    }

    pub(crate) fn validate(&self) -> Result<()> {
        match self.validation_issue() {
            Some(issue) => Err(Error::Message(issue.to_string())),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceFacts {
    pub(crate) latest_version: String,
}

impl SourceFacts {
    fn validate_latest_version(&self) -> Result<()> {
        if self.latest_version.is_empty() || self.latest_version.trim() != self.latest_version {
            return Err(Error::Message(
                "authoritative source returned an invalid latest version".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate(self, provider: EvidenceProvider, identifier: String) -> Result<SourceEvidence> {
        self.validate_latest_version()?;
        Ok(SourceEvidence {
            provider,
            identifier,
            latest_version: self.latest_version,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SourceEvidence {
    pub(crate) provider: EvidenceProvider,
    pub(crate) identifier: String,
    pub(crate) latest_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct GenerationEvidence {
    pub(crate) collected_at_unix_secs: u64,
    pub(crate) update: SourceEvidence,
    pub(crate) latest: SourceEvidence,
}

pub(crate) trait AuthoritativeSources {
    fn homebrew_formula(&self, formula: &str) -> Result<SourceFacts>;
    fn npm_package(&self, package: &str) -> Result<SourceFacts>;
    fn pypi_project(&self, package: &str) -> Result<SourceFacts>;
    fn crate_package(&self, package: &str) -> Result<SourceFacts>;
    fn github_release(&self, repository: &str) -> Result<SourceFacts>;
    fn github_tag(&self, repository: &str) -> Result<SourceFacts>;
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
        facts.validate_latest_version()?;
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

    fn github_tag(&self, repository: &str) -> Result<SourceFacts> {
        self.registry_facts(
            LatestVersionSource::GithubTag {
                repository: repository.to_owned(),
            },
            "GitHub Tag",
        )
    }
}

pub(crate) fn collect_live_evidence(
    request: &GenerationRequest,
    network: &NetworkSettings,
    github_api_key: Option<&str>,
    collected_at_unix_secs: u64,
) -> Result<GenerationEvidence> {
    let sources = LiveAuthoritativeSources::new(version::network_agent(network)?, github_api_key);
    collect_evidence(request, &sources, collected_at_unix_secs)
}

pub(crate) fn collect_evidence(
    request: &GenerationRequest,
    sources: &impl AuthoritativeSources,
    collected_at_unix_secs: u64,
) -> Result<GenerationEvidence> {
    request.validate()?;
    let (update_provider, update_facts) = match request.update_provider {
        UpdateProvider::Homebrew => (
            EvidenceProvider::Homebrew,
            sources.homebrew_formula(&request.update_package)?,
        ),
        UpdateProvider::Npm | UpdateProvider::Pnpm => (
            EvidenceProvider::Npm,
            sources.npm_package(&request.update_package)?,
        ),
        UpdateProvider::Cargo => (
            EvidenceProvider::CratesIo,
            sources.crate_package(&request.update_package)?,
        ),
        UpdateProvider::Pipx | UpdateProvider::Uv => (
            EvidenceProvider::Pypi,
            sources.pypi_project(&request.update_package)?,
        ),
    };
    let update = update_facts.validate(update_provider, request.update_package.clone())?;

    let (latest_provider, latest_identifier, latest_facts) = match &request.latest {
        LatestVersionSource::Npm { package } => (
            EvidenceProvider::Npm,
            package.clone(),
            sources.npm_package(package)?,
        ),
        LatestVersionSource::Pypi { package } => (
            EvidenceProvider::Pypi,
            package.clone(),
            sources.pypi_project(package)?,
        ),
        LatestVersionSource::CratesIo { package } => (
            EvidenceProvider::CratesIo,
            package.clone(),
            sources.crate_package(package)?,
        ),
        LatestVersionSource::GithubRelease { repository } => (
            EvidenceProvider::GithubRelease,
            repository.clone(),
            sources.github_release(repository)?,
        ),
        LatestVersionSource::GithubTag { repository } => (
            EvidenceProvider::GithubTag,
            repository.clone(),
            sources.github_tag(repository)?,
        ),
    };
    let latest = latest_facts.validate(latest_provider, latest_identifier)?;
    Ok(GenerationEvidence {
        collected_at_unix_secs,
        update,
        latest,
    })
}

pub(crate) fn validate_generated_against_evidence(
    request: &GenerationRequest,
    evidence: &GenerationEvidence,
    generated_name: &str,
    generated_tool: &crate::config::UserTool,
) -> Result<()> {
    request.validate()?;
    let expected_latest_provider = match request.latest {
        LatestVersionSource::Npm { .. } => EvidenceProvider::Npm,
        LatestVersionSource::Pypi { .. } => EvidenceProvider::Pypi,
        LatestVersionSource::CratesIo { .. } => EvidenceProvider::CratesIo,
        LatestVersionSource::GithubRelease { .. } => EvidenceProvider::GithubRelease,
        LatestVersionSource::GithubTag { .. } => EvidenceProvider::GithubTag,
    };
    if evidence.update.provider != request.update_provider.evidence_provider()
        || evidence.update.identifier != request.update_package
        || evidence.latest.provider != expected_latest_provider
        || evidence.latest.identifier != request.latest_identifier()
    {
        return Err(Error::Message(
            "authoritative evidence does not match the locked generation request".to_owned(),
        ));
    }
    if generated_name != request.name {
        return Err(Error::Message(format!(
            "AI changed locked tool name `{}` to `{}`",
            request.name, generated_name
        )));
    }
    let expected_update = request
        .update_provider
        .update_command(&request.update_package);
    if generated_tool.update != expected_update {
        return Err(Error::Message(
            "AI changed the locked update command".to_owned(),
        ));
    }
    let expected_update_version = request
        .update_provider
        .update_version_command(&request.update_package);
    if generated_tool.update_version != expected_update_version {
        return Err(Error::Message(
            "AI changed the locked versioned update command".to_owned(),
        ));
    }
    if generated_tool.latest.as_ref() != Some(&request.latest) {
        return Err(Error::Message(
            "AI changed the locked latest-version source".to_owned(),
        ));
    }
    if generated_tool.platforms != request.platforms {
        return Err(Error::Message(
            "AI changed the locked platform list".to_owned(),
        ));
    }
    generated_tool.validate_for_name(generated_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LatestVersionSource;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };

    struct KnownSources;

    impl AuthoritativeSources for KnownSources {
        fn homebrew_formula(&self, formula: &str) -> crate::error::Result<SourceFacts> {
            assert_eq!(formula, "ripgrep");
            Ok(SourceFacts {
                latest_version: "14.1.1".to_owned(),
            })
        }

        fn npm_package(&self, _: &str) -> crate::error::Result<SourceFacts> {
            unreachable!("npm was not selected")
        }

        fn pypi_project(&self, _: &str) -> crate::error::Result<SourceFacts> {
            unreachable!("PyPI was not selected")
        }

        fn crate_package(&self, _: &str) -> crate::error::Result<SourceFacts> {
            unreachable!("crates.io was not selected")
        }

        fn github_release(&self, repository: &str) -> crate::error::Result<SourceFacts> {
            assert_eq!(repository, "BurntSushi/ripgrep");
            Ok(SourceFacts {
                latest_version: "14.1.1".to_owned(),
            })
        }

        fn github_tag(&self, _: &str) -> crate::error::Result<SourceFacts> {
            unreachable!("GitHub tags were not selected")
        }
    }

    #[test]
    fn collects_evidence_only_from_the_explicit_locked_sources() {
        let request = GenerationRequest {
            name: "ripgrep".to_owned(),
            instructions: "Fail when rg is running".to_owned(),
            update_provider: UpdateProvider::Homebrew,
            update_package: "ripgrep".to_owned(),
            latest: LatestVersionSource::GithubRelease {
                repository: "BurntSushi/ripgrep".to_owned(),
            },
            platforms: vec!["macos".to_owned(), "linux".to_owned()],
        };

        let evidence = collect_evidence(&request, &KnownSources, 1_700_000_000)
            .expect("authoritative evidence");

        assert_eq!(evidence.collected_at_unix_secs, 1_700_000_000);
        assert_eq!(evidence.update.provider, EvidenceProvider::Homebrew);
        assert_eq!(evidence.update.identifier, "ripgrep");
        assert_eq!(evidence.update.latest_version, "14.1.1");
        assert_eq!(evidence.latest.provider, EvidenceProvider::GithubRelease);
        assert_eq!(evidence.latest.identifier, "BurntSushi/ripgrep");
        assert_eq!(evidence.latest.latest_version, "14.1.1");
    }

    #[test]
    fn generation_request_rejects_whitespace_padded_source_identifiers() {
        let request = GenerationRequest {
            name: "ripgrep".to_owned(),
            instructions: "Use the verified source".to_owned(),
            update_provider: UpdateProvider::Cargo,
            update_package: "ripgrep".to_owned(),
            latest: LatestVersionSource::Npm {
                package: " ripgrep ".to_owned(),
            },
            platforms: vec!["macos".to_owned()],
        };

        let error = request
            .validate()
            .expect_err("source identifiers must be exact");

        assert!(error.to_string().contains("without surrounding whitespace"));
    }

    #[test]
    fn supported_update_providers_have_deterministic_locked_commands() {
        let cases = [
            (
                UpdateProvider::Homebrew,
                vec!["brew", "upgrade", "example"],
                None,
            ),
            (
                UpdateProvider::Npm,
                vec!["npm", "install", "--global", "example@latest"],
                Some(vec!["npm", "install", "--global", "example@{version}"]),
            ),
            (
                UpdateProvider::Pnpm,
                vec!["pnpm", "add", "--global", "example@latest"],
                Some(vec!["pnpm", "add", "--global", "example@{version}"]),
            ),
            (
                UpdateProvider::Cargo,
                vec!["cargo", "install", "example"],
                Some(vec![
                    "cargo",
                    "install",
                    "example",
                    "--version",
                    "{version}",
                ]),
            ),
            (
                UpdateProvider::Pipx,
                vec!["pipx", "upgrade", "example"],
                Some(vec!["pipx", "install", "--force", "example=={version}"]),
            ),
            (
                UpdateProvider::Uv,
                vec!["uv", "tool", "upgrade", "example"],
                Some(vec![
                    "uv",
                    "tool",
                    "install",
                    "--force",
                    "example=={version}",
                ]),
            ),
        ];

        for (provider, expected_update, expected_versioned_update) in cases {
            assert_eq!(provider.update_command("example"), expected_update);
            assert_eq!(
                provider.update_version_command("example"),
                expected_versioned_update
                    .map(|parts| { parts.into_iter().map(str::to_owned).collect::<Vec<_>>() })
            );
        }
    }

    #[test]
    fn homebrew_evidence_comes_from_verified_formula_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let address = listener.local_addr().expect("listener address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            assert_eq!(
                request_line.trim_end(),
                "GET /formula/ripgrep.json HTTP/1.1"
            );
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("request header");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let body = r#"{"name":"ripgrep","desc":"Fast search tool","versions":{"stable":"14.1.1","bottle":true},"deprecated":false,"disabled":false,"urls":{"stable":{"url":"https://example.invalid/ripgrep.tar.gz"}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("response");
        });
        let sources = LiveAuthoritativeSources::with_homebrew_base_url(
            ureq::Agent::new_with_defaults(),
            None,
            format!("http://{address}/formula"),
        );

        let facts = sources
            .homebrew_formula("ripgrep")
            .expect("verified Homebrew formula");

        assert_eq!(facts.latest_version, "14.1.1");
        server.join().expect("test server");
    }

    #[test]
    fn rejects_ai_output_that_changes_locked_source_fields() {
        let request = GenerationRequest {
            name: "ripgrep".to_owned(),
            instructions: "Use the verified sources".to_owned(),
            update_provider: UpdateProvider::Homebrew,
            update_package: "ripgrep".to_owned(),
            latest: LatestVersionSource::GithubRelease {
                repository: "BurntSushi/ripgrep".to_owned(),
            },
            platforms: vec!["macos".to_owned(), "linux".to_owned()],
        };
        let evidence = GenerationEvidence {
            collected_at_unix_secs: 1_700_000_000,
            update: SourceEvidence {
                provider: EvidenceProvider::Homebrew,
                identifier: "ripgrep".to_owned(),
                latest_version: "14.1.1".to_owned(),
            },
            latest: SourceEvidence {
                provider: EvidenceProvider::GithubRelease,
                identifier: "BurntSushi/ripgrep".to_owned(),
                latest_version: "14.1.1".to_owned(),
            },
        };
        let mut tool = crate::config::UserTool::custom(
            "ripgrep",
            "cargo".to_owned(),
            vec!["install".to_owned(), "ripgrep".to_owned()],
        );
        tool.latest = Some(request.latest.clone());
        tool.platforms = request.platforms.clone();
        let generated = crate::ai::GeneratedCommand {
            name: request.name.clone(),
            tool,
        };

        let error = validate_generated_against_evidence(
            &request,
            &evidence,
            &generated.name,
            &generated.tool,
        )
        .expect_err("AI changed the locked update provider");

        assert!(error.to_string().contains("locked update command"));
    }
}
