use std::{
    fs,
    io::{self, Write},
    net::IpAddr,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{
    credential,
    error::{Error, Result},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Language {
    English,
    Chinese,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProxyMode {
    Environment,
    Explicit,
    Direct,
}

impl ProxyMode {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Environment => Self::Explicit,
            Self::Explicit => Self::Direct,
            Self::Direct => Self::Environment,
        }
    }

    pub(crate) fn previous(self) -> Self {
        match self {
            Self::Environment => Self::Direct,
            Self::Explicit => Self::Environment,
            Self::Direct => Self::Explicit,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Explicit => "explicit",
            Self::Direct => "direct",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetworkSettings {
    pub(crate) proxy_mode: ProxyMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proxy_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) no_proxy: Vec<String>,
    pub(crate) metadata_timeout_secs: u64,
    pub(crate) release_asset_setup_timeout_secs: u64,
    pub(crate) release_asset_body_timeout_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubSettings {
    pub(crate) poll_interval_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) encrypted_api_key: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiSettings {
    #[serde(default = "legacy_ai_enabled")]
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) encrypted_api_key: Option<String>,
}

impl AiSettings {
    fn is_empty(&self) -> bool {
        !self.enabled
            && self.base_url.is_none()
            && self.model.is_none()
            && self.encrypted_api_key.is_none()
    }

    pub(crate) fn configured(&self) -> bool {
        self.enabled && self.connection_ready()
    }

    pub(crate) fn connection_ready(&self) -> bool {
        self.base_url.is_some() && self.model.is_some()
    }

    pub(crate) fn validate_endpoint(&self) -> Result<()> {
        let base_url = self
            .base_url
            .as_deref()
            .ok_or_else(|| Error::InvalidConfig("AI base_url must be configured".to_owned()))?;
        validate_ai_base_url(base_url)?;
        if let Some(encrypted_api_key) = self.encrypted_api_key.as_deref() {
            credential::validate_encrypted_ai_api_key(encrypted_api_key)?;
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(base_url) = self.base_url.as_deref() {
            validate_ai_base_url(base_url)?;
        }
        if let Some(model) = self.model.as_deref()
            && (model.trim() != model || model.is_empty())
        {
            return Err(Error::InvalidConfig(
                "AI model cannot be empty or contain surrounding whitespace".to_owned(),
            ));
        }
        if self.base_url.is_none() && (self.model.is_some() || self.encrypted_api_key.is_some()) {
            return Err(Error::InvalidConfig(
                "AI model and API key require base_url".to_owned(),
            ));
        }
        if self.enabled && !self.connection_ready() {
            return Err(Error::InvalidConfig(
                "enabled AI generation requires base_url and model".to_owned(),
            ));
        }
        if let Some(encrypted_api_key) = self.encrypted_api_key.as_deref() {
            credential::validate_encrypted_ai_api_key(encrypted_api_key)?;
        }
        Ok(())
    }
}

fn legacy_ai_enabled() -> bool {
    true
}

fn validate_ai_base_url(base_url: &str) -> Result<()> {
    if base_url.trim() != base_url || base_url.is_empty() {
        return Err(Error::InvalidConfig(
            "AI base_url cannot be empty or contain surrounding whitespace".to_owned(),
        ));
    }
    let uri = base_url
        .parse::<ureq::http::Uri>()
        .map_err(|error| Error::InvalidConfig(format!("invalid AI base_url: {error}")))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.authority().is_none() {
        return Err(Error::InvalidConfig(
            "AI base_url must be an absolute http:// or https:// URL".to_owned(),
        ));
    }
    let host = uri
        .authority()
        .map(|authority| authority.host())
        .unwrap_or("");
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if uri.scheme_str() == Some("http") && !loopback {
        return Err(Error::InvalidConfig(
            "AI base_url must use https unless it targets a loopback host".to_owned(),
        ));
    }
    if uri
        .path_and_query()
        .and_then(|value| value.query())
        .is_some()
    {
        return Err(Error::InvalidConfig(
            "AI base_url cannot contain a query string".to_owned(),
        ));
    }
    Ok(())
}

impl Default for GithubSettings {
    fn default() -> Self {
        Self {
            poll_interval_secs: 1_800,
            encrypted_api_key: None,
        }
    }
}

impl GithubSettings {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(60..=86_400).contains(&self.poll_interval_secs) {
            return Err(Error::InvalidConfig(
                "github poll_interval_secs must be between 60 and 86400".to_owned(),
            ));
        }
        if let Some(encrypted_api_key) = self.encrypted_api_key.as_deref() {
            credential::validate_encrypted_github_api_key(encrypted_api_key)?;
        }
        Ok(())
    }
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            proxy_mode: ProxyMode::Environment,
            proxy_url: None,
            no_proxy: Vec::new(),
            metadata_timeout_secs: 10,
            release_asset_setup_timeout_secs: 30,
            release_asset_body_timeout_secs: 300,
        }
    }
}

impl NetworkSettings {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=300).contains(&self.metadata_timeout_secs) {
            return Err(Error::InvalidConfig(
                "network metadata_timeout_secs must be between 1 and 300".to_owned(),
            ));
        }
        if !(1..=300).contains(&self.release_asset_setup_timeout_secs) {
            return Err(Error::InvalidConfig(
                "network release_asset_setup_timeout_secs must be between 1 and 300".to_owned(),
            ));
        }
        if !(1..=3_600).contains(&self.release_asset_body_timeout_secs) {
            return Err(Error::InvalidConfig(
                "network release_asset_body_timeout_secs must be between 1 and 3600".to_owned(),
            ));
        }
        if self.release_asset_body_timeout_secs < self.release_asset_setup_timeout_secs {
            return Err(Error::InvalidConfig(
                "network release_asset_body_timeout_secs must be greater than or equal to release_asset_setup_timeout_secs"
                    .to_owned(),
            ));
        }
        match self.proxy_mode {
            ProxyMode::Environment | ProxyMode::Direct => {
                if self.proxy_url.is_some() || !self.no_proxy.is_empty() {
                    return Err(Error::InvalidConfig(format!(
                        "proxy mode `{}` cannot contain proxy_url or no_proxy",
                        self.proxy_mode.label()
                    )));
                }
            }
            ProxyMode::Explicit => {
                let url = self
                    .proxy_url
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        Error::InvalidConfig(
                            "proxy mode `explicit` requires a non-empty proxy_url".to_owned(),
                        )
                    })?;
                if url.trim() != url {
                    return Err(Error::InvalidConfig(
                        "proxy_url cannot contain leading or trailing whitespace".to_owned(),
                    ));
                }
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(Error::InvalidConfig(
                        "proxy_url must start with http:// or https://".to_owned(),
                    ));
                }
                let proxy = ureq::Proxy::new(url)
                    .map_err(|error| Error::InvalidConfig(format!("invalid proxy_url: {error}")))?;
                if !matches!(
                    proxy.protocol(),
                    ureq::ProxyProtocol::Http | ureq::ProxyProtocol::Https
                ) {
                    return Err(Error::InvalidConfig(
                        "proxy_url only supports http:// and https://".to_owned(),
                    ));
                }
                if proxy.username().is_some() || proxy.password().is_some() {
                    return Err(Error::InvalidConfig(
                        "proxy_url credentials are not supported".to_owned(),
                    ));
                }
                if proxy.host().trim().is_empty() {
                    return Err(Error::InvalidConfig(
                        "proxy_url must contain a host".to_owned(),
                    ));
                }
                if proxy
                    .uri()
                    .path_and_query()
                    .is_some_and(|path| path.as_str() != "/")
                {
                    return Err(Error::InvalidConfig(
                        "proxy_url cannot contain a path or query".to_owned(),
                    ));
                }
                for entry in &self.no_proxy {
                    validate_no_proxy_entry(entry)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn explicit_proxy(&self) -> Result<Option<ureq::Proxy>> {
        self.validate()?;
        if self.proxy_mode != ProxyMode::Explicit {
            return Ok(None);
        }
        let url = self.proxy_url.as_deref().expect("validated explicit proxy");
        let parsed = ureq::Proxy::new(url)
            .map_err(|error| Error::InvalidConfig(format!("invalid proxy_url: {error}")))?;
        let mut builder = ureq::Proxy::builder(parsed.protocol())
            .host(parsed.host())
            .port(parsed.port());
        for entry in &self.no_proxy {
            builder = builder.no_proxy(entry);
        }
        builder
            .build()
            .map(Some)
            .map_err(|error| Error::InvalidConfig(format!("invalid proxy_url: {error}")))
    }

    pub(crate) fn no_proxy_value(&self) -> Option<String> {
        (!self.no_proxy.is_empty()).then(|| self.no_proxy.join(","))
    }
}

fn validate_no_proxy_entry(entry: &str) -> Result<()> {
    if entry.is_empty()
        || entry.trim() != entry
        || entry.contains(',')
        || entry.chars().any(char::is_whitespace)
    {
        return Err(Error::InvalidConfig(format!(
            "invalid no_proxy entry `{entry}`"
        )));
    }
    if entry == "*" {
        return Ok(());
    }
    let host = entry
        .strip_prefix("*.")
        .or_else(|| entry.strip_prefix('.'))
        .unwrap_or(entry);
    if host.is_empty()
        || host.starts_with('.')
        || host.ends_with('.')
        || host.split('.').any(|label| {
            label.is_empty()
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
    {
        return Err(Error::InvalidConfig(format!(
            "invalid no_proxy entry `{entry}`"
        )));
    }
    Ok(())
}

/// User-level preferences shared by the CLI, workers, and interactive interface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppSettings {
    pub(crate) language: Language,
    pub(crate) auto_diagnose_on_startup: bool,
    pub(crate) hide_unsupported_and_missing_tools: bool,
    pub(crate) network: NetworkSettings,
    pub(crate) github: GithubSettings,
    #[serde(default, skip_serializing_if = "AiSettings::is_empty")]
    pub(crate) ai: AiSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: Language::English,
            auto_diagnose_on_startup: false,
            hide_unsupported_and_missing_tools: false,
            network: NetworkSettings::default(),
            github: GithubSettings::default(),
            ai: AiSettings::default(),
        }
    }
}

impl AppSettings {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        let settings: Self = toml::from_str(&contents)?;
        settings.network.validate()?;
        settings.github.validate()?;
        settings.ai.validate()?;
        Ok(settings)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        self.network.validate()?;
        self.github.validate()?;
        self.ai.validate()?;
        let contents = toml::to_string_pretty(self)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;

        let mut temporary = tempfile::Builder::new()
            .prefix(".settings.")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file().sync_all()?;
        persist_settings_file(temporary, path)
    }
}

#[cfg(windows)]
fn persist_settings_file(mut temporary: tempfile::NamedTempFile, path: &Path) -> Result<()> {
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
                return Err(Error::SettingsWrite {
                    path: path.to_path_buf(),
                    source: error.error,
                });
            }
        }
    }
}

#[cfg(not(windows))]
fn persist_settings_file(temporary: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| Error::SettingsWrite {
            path: path.to_path_buf(),
            source: error.error,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");

        let missing = AppSettings::load(&path).expect("default settings");
        assert_eq!(missing.language, Language::English);
        assert_eq!(missing.network, NetworkSettings::default());

        let enabled = AppSettings {
            language: Language::Chinese,
            auto_diagnose_on_startup: true,
            hide_unsupported_and_missing_tools: true,
            network: NetworkSettings {
                proxy_mode: ProxyMode::Explicit,
                proxy_url: Some("http://127.0.0.1:7890".to_owned()),
                no_proxy: vec!["localhost".to_owned(), ".example.com".to_owned()],
                metadata_timeout_secs: 17,
                release_asset_setup_timeout_secs: 41,
                release_asset_body_timeout_secs: 523,
            },
            github: GithubSettings::default(),
            ai: AiSettings::default(),
        };
        enabled.save(&path).expect("save settings");

        assert_eq!(AppSettings::load(&path).expect("reload settings"), enabled);
        let serialized = fs::read_to_string(path).expect("read settings file");
        assert!(!serialized.contains("version ="));
        assert!(serialized.contains("[network]"));
        assert!(serialized.contains("proxy_mode = \"explicit\""));
        assert!(serialized.contains("metadata_timeout_secs = 17"));
        assert!(serialized.contains("release_asset_setup_timeout_secs = 41"));
        assert!(serialized.contains("release_asset_body_timeout_secs = 523"));
        assert!(!serialized.contains("api_key"));
    }

    #[test]
    fn network_timeout_fields_are_required_by_the_serialized_schema() {
        let serialized = toml::to_string(&NetworkSettings::default()).expect("network settings");

        for required_field in [
            "metadata_timeout_secs",
            "release_asset_setup_timeout_secs",
            "release_asset_body_timeout_secs",
        ] {
            let incomplete = serialized
                .lines()
                .filter(|line| !line.starts_with(required_field))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                toml::from_str::<NetworkSettings>(&incomplete).is_err(),
                "missing {required_field} was accepted"
            );
        }
    }

    #[test]
    fn network_timeout_values_are_strictly_validated() {
        let defaults = NetworkSettings::default();
        assert_eq!(defaults.metadata_timeout_secs, 10);
        assert_eq!(defaults.release_asset_setup_timeout_secs, 30);
        assert_eq!(defaults.release_asset_body_timeout_secs, 300);
        assert!(defaults.validate().is_ok());

        for invalid in [
            NetworkSettings {
                metadata_timeout_secs: 0,
                ..defaults.clone()
            },
            NetworkSettings {
                metadata_timeout_secs: 301,
                ..defaults.clone()
            },
            NetworkSettings {
                release_asset_setup_timeout_secs: 0,
                ..defaults.clone()
            },
            NetworkSettings {
                release_asset_setup_timeout_secs: 301,
                ..defaults.clone()
            },
            NetworkSettings {
                release_asset_body_timeout_secs: 0,
                ..defaults.clone()
            },
            NetworkSettings {
                release_asset_body_timeout_secs: 3_601,
                ..defaults.clone()
            },
            NetworkSettings {
                release_asset_setup_timeout_secs: 60,
                release_asset_body_timeout_secs: 59,
                ..defaults
            },
        ] {
            assert!(invalid.validate().is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn settings_without_ai_section_remain_backward_compatible() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");
        let contents = concat!(
            "language = \"english\"\n",
            "auto_diagnose_on_startup = false\n",
            "hide_unsupported_and_missing_tools = false\n",
            "\n[network]\n",
            "proxy_mode = \"environment\"\n",
            "metadata_timeout_secs = 10\n",
            "release_asset_setup_timeout_secs = 30\n",
            "release_asset_body_timeout_secs = 300\n",
            "\n[github]\n",
            "poll_interval_secs = 1800\n",
        );
        fs::write(&path, contents).expect("legacy settings");

        let settings = AppSettings::load(&path).expect("load settings without AI");

        assert_eq!(settings.ai, AiSettings::default());
        settings.save(&path).expect("save settings without AI");
        assert!(!fs::read_to_string(path).unwrap().contains("[ai]"));
    }

    #[test]
    fn ai_switch_defaults_off_but_legacy_configured_sections_remain_on() {
        assert!(!AiSettings::default().enabled);

        let contents = concat!(
            "language = \"english\"\n",
            "auto_diagnose_on_startup = false\n",
            "hide_unsupported_and_missing_tools = false\n",
            "\n[network]\n",
            "proxy_mode = \"environment\"\n",
            "metadata_timeout_secs = 10\n",
            "release_asset_setup_timeout_secs = 30\n",
            "release_asset_body_timeout_secs = 300\n",
            "\n[github]\n",
            "poll_interval_secs = 1800\n",
            "\n[ai]\n",
            "base_url = \"https://ai.example.com/v1\"\n",
            "model = \"example-model\"\n",
        );

        let settings: AppSettings = toml::from_str(contents).expect("legacy AI settings");

        assert!(settings.ai.enabled);
        assert!(settings.ai.configured());
    }

    #[test]
    fn ai_settings_require_a_valid_base_url_and_model_pair() {
        let configured = AiSettings {
            enabled: true,
            base_url: Some("https://ai.example.com/v1".to_owned()),
            model: Some("example-model".to_owned()),
            encrypted_api_key: None,
        };
        assert!(configured.validate().is_ok());
        assert!(
            AiSettings {
                enabled: true,
                base_url: Some("http://127.0.0.1:11434/v1".to_owned()),
                model: Some("local-model".to_owned()),
                encrypted_api_key: None,
            }
            .validate()
            .is_ok()
        );

        for invalid in [
            AiSettings {
                enabled: true,
                base_url: Some("https://ai.example.com/v1".to_owned()),
                model: None,
                encrypted_api_key: None,
            },
            AiSettings {
                enabled: true,
                base_url: Some("file:///tmp/model".to_owned()),
                model: Some("example-model".to_owned()),
                encrypted_api_key: None,
            },
            AiSettings {
                enabled: true,
                base_url: Some("https://ai.example.com/v1".to_owned()),
                model: Some(" model ".to_owned()),
                encrypted_api_key: None,
            },
            AiSettings {
                enabled: true,
                base_url: Some("http://ai.example.com/v1".to_owned()),
                model: Some("example-model".to_owned()),
                encrypted_api_key: None,
            },
        ] {
            assert!(invalid.validate().is_err(), "invalid settings: {invalid:?}");
        }
    }

    #[test]
    fn encrypted_github_api_key_persists_without_writing_plaintext() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");
        let token = "github_pat_settings_round_trip_test";
        let mut settings = AppSettings::default();
        settings.github.encrypted_api_key =
            Some(credential::encrypt_github_api_key(token).expect("encrypt GitHub API key"));

        settings.save(&path).expect("save encrypted settings");

        let serialized = fs::read_to_string(&path).expect("read encrypted settings");
        assert!(serialized.contains("encrypted_api_key"));
        assert!(!serialized.contains(token));
        let reloaded = AppSettings::load(&path).expect("reload encrypted settings");
        assert_eq!(reloaded, settings);
        assert_eq!(
            credential::github_api_key(reloaded.github.encrypted_api_key.as_deref())
                .expect("decrypt reloaded GitHub API key")
                .expect("configured GitHub API key")
                .as_str(),
            token
        );
    }

    #[test]
    fn settings_reject_github_release_monitors() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");
        let target = toml::Value::String(temporary.path().join("tool").display().to_string());
        let contents = format!(
            "{}\n[[github.monitors]]\nname = \"example\"\nrepository = \"owner/repository\"\nasset_regex = '^tool-[0-9.]+\\.zip$'\ntarget_directory = {target}\nformat = \"zip\"\nmax_download_bytes = 1024\nmax_extracted_bytes = 2048\nmax_extracted_files = 10\nenabled = true\n",
            toml::to_string_pretty(&AppSettings::default()).expect("serialize settings")
        );
        fs::write(&path, contents).expect("write settings with misplaced monitors");

        assert!(AppSettings::load(&path).is_err());
    }

    #[test]
    fn settings_default_only_when_the_file_is_missing() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");

        assert_eq!(
            AppSettings::load(&path).expect("missing settings use defaults"),
            AppSettings::default()
        );

        fs::create_dir(&path).expect("settings directory");
        assert!(matches!(AppSettings::load(&path), Err(Error::Io(_))));
        assert!(path.is_dir());
    }

    #[test]
    fn settings_save_replaces_the_file_and_cleans_up_temporary_files() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");
        fs::write(&path, "old contents").expect("old settings");

        let settings = AppSettings {
            language: Language::Chinese,
            ..AppSettings::default()
        };
        settings.save(&path).expect("replace settings");

        assert_eq!(AppSettings::load(&path).expect("new settings"), settings);
        assert_eq!(
            fs::read_dir(temporary.path())
                .expect("settings directory")
                .count(),
            1
        );
    }

    #[test]
    fn failed_settings_save_preserves_the_destination_and_cleans_up() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");
        fs::create_dir(&path).expect("settings directory");
        fs::write(path.join("sentinel"), "keep me").expect("sentinel");

        assert!(AppSettings::default().save(&path).is_err());
        assert_eq!(
            fs::read_to_string(path.join("sentinel")).expect("preserved destination"),
            "keep me"
        );
        assert_eq!(
            fs::read_dir(temporary.path())
                .expect("settings directory")
                .count(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn locked_windows_settings_report_the_exact_path_and_succeed_after_release() {
        use std::os::windows::fs::OpenOptionsExt;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");
        let original = AppSettings::default();
        original.save(&path).expect("initial settings");
        let original_contents = fs::read_to_string(&path).expect("original contents");
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x0000_0001 | 0x0000_0002)
            .open(&path)
            .expect("open settings without delete sharing");
        let changed = AppSettings {
            language: Language::Chinese,
            ..AppSettings::default()
        };

        let error = changed.save(&path).expect_err("locked settings must fail");

        assert!(matches!(
            error,
            Error::SettingsWrite {
                path: ref failed_path,
                ..
            } if failed_path == &path
        ));
        assert_eq!(
            fs::read_to_string(&path).expect("preserved locked settings"),
            original_contents
        );
        drop(lock);
        changed.save(&path).expect("save after releasing lock");
        assert_eq!(
            AppSettings::load(&path).expect("reloaded settings"),
            changed
        );
    }

    #[test]
    fn strict_proxy_modes_reject_inconsistent_fields() {
        let explicit_without_url = NetworkSettings {
            proxy_mode: ProxyMode::Explicit,
            proxy_url: None,
            no_proxy: Vec::new(),
            ..NetworkSettings::default()
        };
        assert!(explicit_without_url.validate().is_err());

        for proxy_mode in [ProxyMode::Environment, ProxyMode::Direct] {
            let invalid = NetworkSettings {
                proxy_mode,
                proxy_url: Some("http://127.0.0.1:7890".to_owned()),
                no_proxy: vec!["localhost".to_owned()],
                ..NetworkSettings::default()
            };
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn explicit_proxy_rejects_credentials_socks_and_invalid_no_proxy() {
        for proxy_url in [
            "http://user:secret@127.0.0.1:7890",
            "socks5://127.0.0.1:7890",
            "127.0.0.1:7890",
            "http://127.0.0.1:7890/path",
        ] {
            let invalid = NetworkSettings {
                proxy_mode: ProxyMode::Explicit,
                proxy_url: Some(proxy_url.to_owned()),
                no_proxy: Vec::new(),
                ..NetworkSettings::default()
            };
            assert!(invalid.validate().is_err(), "accepted {proxy_url}");
        }

        let invalid = NetworkSettings {
            proxy_mode: ProxyMode::Explicit,
            proxy_url: Some("https://proxy.example.com:443".to_owned()),
            no_proxy: vec!["bad host".to_owned()],
            ..NetworkSettings::default()
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn settings_reject_unknown_fields_obsolete_version_and_missing_network() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");

        fs::write(
            &path,
            "language = \"english\"\nauto_diagnose_on_startup = false\nhide_unsupported_and_missing_tools = false\nunknown = true\n\n[network]\nproxy_mode = \"environment\"\n",
        )
        .expect("unknown settings");
        assert!(AppSettings::load(&path).is_err());

        fs::write(
            &path,
            "version = 3\nlanguage = \"english\"\nauto_diagnose_on_startup = false\nhide_unsupported_and_missing_tools = false\n\n[network]\nproxy_mode = \"environment\"\n",
        )
        .expect("obsolete settings version");
        assert!(AppSettings::load(&path).is_err());

        fs::write(
            &path,
            "language = \"english\"\nauto_diagnose_on_startup = false\nhide_unsupported_and_missing_tools = false\n",
        )
        .expect("missing network");
        assert!(AppSettings::load(&path).is_err());

        fs::write(
            &path,
            "auto_diagnose_on_startup = false\nhide_unsupported_and_missing_tools = false\n\n[network]\nproxy_mode = \"environment\"\n",
        )
        .expect("missing language");
        assert!(AppSettings::load(&path).is_err());
    }

    #[test]
    fn settings_reject_unknown_language_and_network_fields() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let path = temporary.path().join("settings.toml");

        fs::write(
            &path,
            "language = \"unknown\"\nauto_diagnose_on_startup = false\nhide_unsupported_and_missing_tools = false\n\n[network]\nproxy_mode = \"environment\"\n",
        )
        .expect("unknown language");
        assert!(AppSettings::load(&path).is_err());

        fs::write(
            &path,
            "language = \"english\"\nauto_diagnose_on_startup = false\nhide_unsupported_and_missing_tools = false\n\n[network]\nproxy_mode = \"environment\"\nunknown = true\n",
        )
        .expect("unknown network setting");
        assert!(AppSettings::load(&path).is_err());
    }
}
