use std::{collections::HashSet, env, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    credential,
    error::{Error, Result},
    generation::{CommandCandidate, GithubCandidate},
    settings::{AiSettings, NetworkSettings, ProxyMode},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const CONNECTION_TEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_AI_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_GENERATION_CANDIDATES: usize = 5;
const USER_AGENT: &str = concat!("dvup/", env!("CARGO_PKG_VERSION"));
const PACKAGE_MANAGERS: &[&str] = &["brew", "npm", "pnpm", "cargo", "pipx", "uv"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SystemContext {
    pub(crate) os: &'static str,
    pub(crate) arch: &'static str,
    pub(crate) available_package_managers: Vec<&'static str>,
}

impl SystemContext {
    pub(crate) fn detect() -> Self {
        Self {
            os: env::consts::OS,
            arch: env::consts::ARCH,
            available_package_managers: PACKAGE_MANAGERS
                .iter()
                .copied()
                .filter(|program| program_on_path(program))
                .collect(),
        }
    }

    fn prompt(&self) -> String {
        format!(
            "operating_system={}\narchitecture={}\navailable_package_managers={}",
            self.os,
            self.arch,
            self.available_package_managers.join(","),
        )
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
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandCandidatesResponse {
    candidates: Vec<CommandCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubCandidatesResponse {
    candidates: Vec<GithubCandidate>,
}

pub(crate) fn analyze_command_intent(
    settings: &AiSettings,
    network: &NetworkSettings,
    context: &SystemContext,
    intent: &str,
) -> Result<Vec<CommandCandidate>> {
    validate_analysis_request(settings, intent)?;
    let intent = intent.trim();
    let system = concat!(
        "You analyze a command-tool update intent and propose 1 to 5 command candidates. ",
        "Return one JSON object only with exactly one property, candidates. Each candidate has ",
        "exactly name, manager, and package. manager must be homebrew, npm, pnpm, cargo, pipx, ",
        "or uv. Never return a GitHub repository monitor, latest-version source, raw command, ",
        "TOML, webpage, README, installer script, markdown, or explanation. If the request is ",
        "specifically for repository asset monitoring, return an empty candidates array."
    );
    let response = chat_text(
        settings,
        network,
        system,
        &analysis_user_prompt(context, intent)?,
    )?;
    let response = parse_command_candidates(&response)?;
    validate_command_candidates(response.candidates)
}

pub(crate) fn analyze_github_intent(
    settings: &AiSettings,
    network: &NetworkSettings,
    context: &SystemContext,
    intent: &str,
) -> Result<Vec<GithubCandidate>> {
    validate_analysis_request(settings, intent)?;
    let intent = intent.trim();
    let system = concat!(
        "You analyze a GitHub repository monitoring intent and propose 1 to 5 repository ",
        "candidates. Return one JSON object only with exactly one property, candidates. Each ",
        "candidate has exactly name and repository in owner/name form. name uses only ASCII ",
        "letters, digits, dash, underscore, or dot. Never return a command candidate, package ",
        "manager, asset regex, install path, TOML, markdown, or explanation. If the request is ",
        "specifically for a package-manager command, return an empty candidates array."
    );
    let response = chat_text(
        settings,
        network,
        system,
        &analysis_user_prompt(context, intent)?,
    )?;
    let response = parse_github_candidates(&response)?;
    validate_github_candidates(response.candidates)
}

fn validate_analysis_request(settings: &AiSettings, intent: &str) -> Result<()> {
    if !settings.configured() {
        return Err(Error::Message(
            "AI generation is disabled or incomplete".to_owned(),
        ));
    }
    let intent = intent.trim();
    if intent.is_empty() {
        return Err(Error::Message(
            "tool update description must be non-empty".to_owned(),
        ));
    }
    Ok(())
}

fn analysis_user_prompt(context: &SystemContext, intent: &str) -> Result<String> {
    Ok(format!(
        "Analyze this tool update request.\n{}\nintent={}",
        context.prompt(),
        serde_json::to_string(intent)?,
    ))
}

fn parse_command_candidates(response: &str) -> Result<CommandCandidatesResponse> {
    serde_json::from_str(response).map_err(|error| {
        if response_has_candidate_field(response, "repository") {
            Error::Message(
                "AI returned a GitHub monitor candidate; use the GitHub repository view".to_owned(),
            )
        } else {
            Error::Message(format!("AI command candidate response is invalid: {error}"))
        }
    })
}

fn parse_github_candidates(response: &str) -> Result<GithubCandidatesResponse> {
    serde_json::from_str(response).map_err(|error| {
        if response_has_candidate_field(response, "manager")
            || response_has_candidate_field(response, "package")
        {
            Error::Message("AI returned a command candidate; use the command tools view".to_owned())
        } else {
            Error::Message(format!("AI GitHub candidate response is invalid: {error}"))
        }
    })
}

fn response_has_candidate_field(response: &str, field: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(response)
        .ok()
        .and_then(|value| {
            value
                .get("candidates")
                .and_then(|value| value.as_array())
                .cloned()
        })
        .is_some_and(|candidates| {
            candidates
                .iter()
                .any(|candidate| candidate.get(field).is_some())
        })
}

fn validate_command_candidates(candidates: Vec<CommandCandidate>) -> Result<Vec<CommandCandidate>> {
    if candidates.is_empty() {
        return Err(Error::Message(
            "request belongs in the GitHub repository view, or no command candidate was found"
                .to_owned(),
        ));
    }
    validate_candidate_count(candidates.len())?;
    let mut accepted = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        candidate.validate()?;
        if accepted.contains(&candidate) {
            return Err(Error::Message(
                "AI returned duplicate command candidates".to_owned(),
            ));
        }
        accepted.push(candidate);
    }
    Ok(accepted)
}

fn validate_github_candidates(candidates: Vec<GithubCandidate>) -> Result<Vec<GithubCandidate>> {
    if candidates.is_empty() {
        return Err(Error::Message(
            "request belongs in the command tools view, or no GitHub repository was found"
                .to_owned(),
        ));
    }
    validate_candidate_count(candidates.len())?;
    let mut accepted = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        candidate.validate()?;
        if accepted.contains(&candidate) {
            return Err(Error::Message(
                "AI returned duplicate GitHub candidates".to_owned(),
            ));
        }
        accepted.push(candidate);
    }
    Ok(accepted)
}

fn validate_candidate_count(count: usize) -> Result<()> {
    if count > MAX_GENERATION_CANDIDATES {
        return Err(Error::Message(format!(
            "AI must return 1 to {MAX_GENERATION_CANDIDATES} tool candidates"
        )));
    }
    Ok(())
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
    use crate::config::PackageManager;

    #[test]
    fn appends_chat_completions_once() {
        assert_eq!(
            chat_completions_endpoint("https://example.test/v1"),
            "https://example.test/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_endpoint("https://example.test/v1/chat/completions"),
            "https://example.test/v1/chat/completions"
        );
    }

    #[test]
    fn extracts_text_from_string_and_content_parts() {
        assert_eq!(
            response_content_text(&serde_json::json!("plain")).expect("plain text"),
            "plain"
        );
        assert_eq!(
            response_content_text(&serde_json::json!([
                {"type": "output_text", "text": "one"},
                {"type": "output_text", "text": "two"}
            ]))
            .expect("content parts"),
            "onetwo"
        );
    }

    #[test]
    fn command_protocol_accepts_only_package_identity_fields() {
        let response = parse_command_candidates(
            r#"{"candidates":[{"name":"ripgrep","manager":"cargo","package":"ripgrep"}]}"#,
        )
        .expect("command candidates");

        assert_eq!(
            response.candidates,
            vec![CommandCandidate {
                name: "ripgrep".to_owned(),
                manager: PackageManager::Cargo,
                package: "ripgrep".to_owned(),
            }]
        );
        assert!(
            parse_command_candidates(
                r#"{"candidates":[{"name":"ripgrep","manager":"cargo","package":"ripgrep","latest":{"provider":"github_release","repository":"owner/repo"}}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn command_protocol_rejects_github_candidates_with_view_hint() {
        let error = parse_command_candidates(
            r#"{"candidates":[{"name":"ripgrep","repository":"BurntSushi/ripgrep"}]}"#,
        )
        .expect_err("GitHub candidate must not enter command protocol");

        assert!(error.to_string().contains("GitHub repository view"));
    }

    #[test]
    fn github_protocol_accepts_only_repository_identity_fields() {
        let response = parse_github_candidates(
            r#"{"candidates":[{"name":"ripgrep","repository":"BurntSushi/ripgrep"}]}"#,
        )
        .expect("GitHub candidates");

        assert_eq!(
            response.candidates,
            vec![GithubCandidate {
                name: "ripgrep".to_owned(),
                repository: "BurntSushi/ripgrep".to_owned(),
            }]
        );
        assert!(
            parse_github_candidates(
                r#"{"candidates":[{"name":"ripgrep","repository":"BurntSushi/ripgrep","asset_regex":"ripgrep.*"}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn github_protocol_rejects_command_candidates_with_view_hint() {
        let error = parse_github_candidates(
            r#"{"candidates":[{"name":"ripgrep","manager":"cargo","package":"ripgrep"}]}"#,
        )
        .expect_err("command candidate must not enter GitHub protocol");

        assert!(error.to_string().contains("command tools view"));
    }

    #[test]
    fn scoped_candidate_lists_reject_empty_and_duplicate_results() {
        let duplicate = CommandCandidate {
            name: "ripgrep".to_owned(),
            manager: PackageManager::Cargo,
            package: "ripgrep".to_owned(),
        };
        assert!(validate_command_candidates(Vec::new()).is_err());
        assert!(validate_command_candidates(vec![duplicate.clone(), duplicate]).is_err());
        assert!(validate_github_candidates(Vec::new()).is_err());
    }
}
