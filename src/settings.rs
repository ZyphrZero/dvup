use std::{
    fs,
    io::{self, Write},
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubSettings {
    pub(crate) poll_interval_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) encrypted_api_key: Option<String>,
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
        }
    }
}

impl NetworkSettings {
    pub(crate) fn validate(&self) -> Result<()> {
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
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: Language::English,
            auto_diagnose_on_startup: false,
            hide_unsupported_and_missing_tools: false,
            network: NetworkSettings::default(),
            github: GithubSettings::default(),
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
        Ok(settings)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        self.network.validate()?;
        self.github.validate()?;
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
            },
            github: GithubSettings::default(),
        };
        enabled.save(&path).expect("save settings");

        assert_eq!(AppSettings::load(&path).expect("reload settings"), enabled);
        let serialized = fs::read_to_string(path).expect("read settings file");
        assert!(!serialized.contains("version ="));
        assert!(serialized.contains("[network]"));
        assert!(serialized.contains("proxy_mode = \"explicit\""));
        assert!(!serialized.contains("api_key"));
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
        };
        assert!(explicit_without_url.validate().is_err());

        for proxy_mode in [ProxyMode::Environment, ProxyMode::Direct] {
            let invalid = NetworkSettings {
                proxy_mode,
                proxy_url: Some("http://127.0.0.1:7890".to_owned()),
                no_proxy: vec!["localhost".to_owned()],
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
            };
            assert!(invalid.validate().is_err(), "accepted {proxy_url}");
        }

        let invalid = NetworkSettings {
            proxy_mode: ProxyMode::Explicit,
            proxy_url: Some("https://proxy.example.com:443".to_owned()),
            no_proxy: vec!["bad host".to_owned()],
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
