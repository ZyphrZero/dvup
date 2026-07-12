use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    config::{GithubReleaseMonitor, ReleaseAssetFormat},
    credential,
    error::{Error, Result},
    settings::{GithubSettings, NetworkSettings},
    version,
};

const USER_AGENT: &str = concat!("dvup/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MonitorOutcome {
    Current {
        name: String,
        tag: String,
    },
    Updated {
        name: String,
        tag: String,
        asset: String,
    },
    Failed {
        name: String,
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MonitorStatus {
    pub(crate) name: String,
    pub(crate) installed_tag: Option<String>,
    pub(crate) latest_tag: Option<String>,
    pub(crate) asset: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    url: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseState {
    releases: BTreeMap<String, String>,
}

pub(crate) fn probe_monitors(
    monitors: &[GithubReleaseMonitor],
    github: &GithubSettings,
    network: &NetworkSettings,
    state_path: &Path,
) -> Result<Vec<MonitorStatus>> {
    github.validate()?;
    let state = load_state(state_path)?;
    let enabled = monitors.iter().any(|monitor| monitor.enabled);
    let agent = enabled
        .then(|| version::network_agent(network))
        .transpose()?;
    let api_key = enabled
        .then(|| credential::github_api_key(github.encrypted_api_key.as_deref()))
        .transpose()?
        .flatten();
    Ok(monitors
        .iter()
        .map(|monitor| {
            let installed_tag = state.releases.get(&monitor.name).cloned();
            if !monitor.enabled {
                return MonitorStatus {
                    name: monitor.name.clone(),
                    installed_tag,
                    latest_tag: None,
                    asset: None,
                    error: None,
                };
            }
            match resolve_latest_asset(
                monitor,
                api_key.as_ref().map(|key| key.as_str()),
                agent
                    .as_ref()
                    .expect("enabled monitors create a network agent"),
            ) {
                Ok(release) => MonitorStatus {
                    name: monitor.name.clone(),
                    installed_tag,
                    latest_tag: Some(release.tag),
                    asset: Some(release.asset.name),
                    error: None,
                },
                Err(error) => MonitorStatus {
                    name: monitor.name.clone(),
                    installed_tag,
                    latest_tag: None,
                    asset: None,
                    error: Some(error.to_string()),
                },
            }
        })
        .collect())
}

pub(crate) fn run_selected_monitors(
    monitors: &[GithubReleaseMonitor],
    github: &GithubSettings,
    network: &NetworkSettings,
    state_path: &Path,
    names: &[String],
) -> Result<Vec<MonitorOutcome>> {
    github.validate()?;
    let agent = version::network_agent(network)?;
    let api_key = credential::github_api_key(github.encrypted_api_key.as_deref())?;
    let mut state = load_state(state_path)?;
    let mut changed = false;
    let outcomes = monitors
        .iter()
        .filter(|monitor| monitor.enabled && names.iter().any(|name| name == &monitor.name))
        .map(|monitor| {
            match update_monitor(
                monitor,
                api_key.as_ref().map(|key| key.as_str()),
                &agent,
                &state,
            ) {
                Ok(MonitorOutcome::Updated { name, tag, asset }) => {
                    state.releases.insert(name.clone(), tag.clone());
                    changed = true;
                    MonitorOutcome::Updated { name, tag, asset }
                }
                Ok(outcome) => outcome,
                Err(error) => MonitorOutcome::Failed {
                    name: monitor.name.clone(),
                    error: error.to_string(),
                },
            }
        })
        .collect::<Vec<_>>();
    if changed {
        save_state(state_path, &state)?;
    }
    Ok(outcomes)
}

fn update_monitor(
    monitor: &GithubReleaseMonitor,
    api_key: Option<&str>,
    agent: &ureq::Agent,
    state: &ReleaseState,
) -> Result<MonitorOutcome> {
    let release = resolve_latest_asset(monitor, api_key, agent)?;
    if state.releases.get(&monitor.name) == Some(&release.tag) {
        return Ok(MonitorOutcome::Current {
            name: monitor.name.clone(),
            tag: release.tag,
        });
    }

    install_asset(monitor, api_key, agent, &release.asset)?;

    Ok(MonitorOutcome::Updated {
        name: monitor.name.clone(),
        tag: release.tag,
        asset: release.asset.name,
    })
}

struct ResolvedRelease {
    tag: String,
    asset: GithubAsset,
}

fn resolve_latest_asset(
    monitor: &GithubReleaseMonitor,
    api_key: Option<&str>,
    agent: &ureq::Agent,
) -> Result<ResolvedRelease> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        monitor.repository
    );
    let mut response = github_get(agent, &url, api_key, "application/vnd.github+json")?;
    let release = response
        .body_mut()
        .read_json::<GithubRelease>()
        .map_err(release_error)?;
    let asset_regex = Regex::new(&monitor.asset_regex).map_err(release_error)?;
    let mut assets = release
        .assets
        .into_iter()
        .filter(|asset| asset_regex.is_match(&asset.name))
        .collect::<Vec<_>>();
    if assets.len() != 1 {
        return Err(Error::Message(format!(
            "GitHub release monitor `{}` expected exactly one asset matching regex `{}`, found {}",
            monitor.name,
            monitor.asset_regex,
            assets.len()
        )));
    }
    let asset = assets.pop().expect("one matching asset");
    if Path::new(&asset.name)
        .file_name()
        .and_then(|name| name.to_str())
        != Some(asset.name.as_str())
    {
        return Err(Error::Message(format!(
            "GitHub release monitor `{}` returned an unsafe asset name",
            monitor.name
        )));
    }

    Ok(ResolvedRelease {
        tag: release.tag_name,
        asset,
    })
}

fn install_asset(
    monitor: &GithubReleaseMonitor,
    api_key: Option<&str>,
    agent: &ureq::Agent,
    asset: &GithubAsset,
) -> Result<()> {
    fs::create_dir_all(&monitor.target_directory)?;
    let parent = monitor
        .target_directory
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            Error::Message(format!(
                "GitHub release monitor `{}` target has no parent directory",
                monitor.name
            ))
        })?;
    fs::create_dir_all(parent)?;
    let mut downloaded = tempfile::Builder::new()
        .prefix(".dvup-release-")
        .suffix(".download")
        .tempfile_in(parent)?;
    let mut download = github_get(agent, &asset.url, api_key, "application/octet-stream")?;
    let copied = io::copy(
        &mut download
            .body_mut()
            .as_reader()
            .take(monitor.max_download_bytes.saturating_add(1)),
        &mut downloaded,
    )?;
    if copied > monitor.max_download_bytes {
        return Err(Error::Message(format!(
            "GitHub release monitor `{}` asset exceeds max_download_bytes",
            monitor.name
        )));
    }
    downloaded.flush()?;
    downloaded.as_file().sync_all()?;

    match monitor.format {
        ReleaseAssetFormat::File => {
            install_file(downloaded, &monitor.target_directory, &asset.name)?
        }
        ReleaseAssetFormat::Zip => install_archive(
            &monitor.target_directory,
            monitor.strip_components,
            |staging| {
                extract_zip(
                    downloaded.path(),
                    staging,
                    monitor.strip_components,
                    monitor.max_extracted_bytes,
                    monitor.max_extracted_files,
                )
            },
        )?,
        ReleaseAssetFormat::TarGz => install_archive(
            &monitor.target_directory,
            monitor.strip_components,
            |staging| {
                extract_tar_gz(
                    downloaded.path(),
                    staging,
                    monitor.strip_components,
                    monitor.max_extracted_bytes,
                    monitor.max_extracted_files,
                )
            },
        )?,
    }

    Ok(())
}

fn github_get(
    agent: &ureq::Agent,
    url: &str,
    api_key: Option<&str>,
    accept: &str,
) -> Result<ureq::http::Response<ureq::Body>> {
    let request = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28");
    let request = if let Some(api_key) = api_key {
        request.header("Authorization", &format!("Bearer {api_key}"))
    } else {
        request
    };
    request.call().map_err(release_error)
}

fn install_file(
    downloaded: tempfile::NamedTempFile,
    target_directory: &Path,
    asset_name: &str,
) -> Result<()> {
    let target = target_directory.join(asset_name);
    if target.exists() {
        fs::remove_file(&target)?;
    }
    downloaded.persist(&target).map_err(|error| {
        Error::Message(format!("failed to install {}: {}", target.display(), error))
    })?;
    Ok(())
}

fn install_archive(
    target_directory: &Path,
    _strip_components: usize,
    extract: impl FnOnce(&Path) -> Result<usize>,
) -> Result<()> {
    let parent = target_directory.parent().ok_or_else(|| {
        Error::Message(format!(
            "archive target `{}` has no parent directory",
            target_directory.display()
        ))
    })?;
    let staging_owner = tempfile::Builder::new()
        .prefix(".dvup-extract-")
        .tempdir_in(parent)?;
    let staging = staging_owner.path().join("contents");
    fs::create_dir(&staging)?;
    if extract(&staging)? == 0 {
        return Err(Error::Message(
            "release archive contained no files".to_owned(),
        ));
    }

    let backup_owner = tempfile::Builder::new()
        .prefix(".dvup-backup-")
        .tempdir_in(parent)?;
    let backup = backup_owner.keep();
    fs::remove_dir(&backup)?;
    let had_target = target_directory.exists();
    if had_target {
        fs::rename(target_directory, &backup)?;
    }
    if let Err(error) = fs::rename(&staging, target_directory) {
        if had_target {
            let _ = fs::rename(&backup, target_directory);
        }
        return Err(error.into());
    }
    if had_target {
        fs::remove_dir_all(backup)?;
    }
    Ok(())
}

fn extract_zip(
    archive: &Path,
    target: &Path,
    strip_components: usize,
    max_extracted_bytes: u64,
    max_extracted_files: usize,
) -> Result<usize> {
    let file = File::open(archive)?;
    let mut archive = zip::ZipArchive::new(file).map_err(release_error)?;
    let mut extracted_files = 0;
    let mut extracted_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(release_error)?;
        let source = entry
            .enclosed_name()
            .ok_or_else(|| Error::Message(format!("unsafe ZIP entry `{}`", entry.name())))?;
        let Some(relative) = stripped_path(&source, strip_components)? else {
            continue;
        };
        let destination = target.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(destination)?;
            continue;
        }
        if extracted_files >= max_extracted_files {
            return Err(Error::Message(
                "release ZIP exceeds max_extracted_files".to_owned(),
            ));
        }
        let remaining = max_extracted_bytes.saturating_sub(extracted_bytes);
        if entry.size() > remaining {
            return Err(Error::Message(
                "release ZIP exceeds max_extracted_bytes".to_owned(),
            ));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(destination)?;
        let copied = io::copy(
            &mut entry.by_ref().take(remaining.saturating_add(1)),
            &mut output,
        )?;
        if copied > remaining {
            return Err(Error::Message(
                "release ZIP exceeds max_extracted_bytes".to_owned(),
            ));
        }
        output.flush()?;
        extracted_bytes += copied;
        extracted_files += 1;
    }
    Ok(extracted_files)
}

fn extract_tar_gz(
    archive: &Path,
    target: &Path,
    strip_components: usize,
    max_extracted_bytes: u64,
    max_extracted_files: usize,
) -> Result<usize> {
    let file = File::open(archive)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut extracted_files = 0;
    let mut extracted_bytes = 0_u64;
    for entry in archive.entries().map_err(release_error)? {
        let mut entry = entry.map_err(release_error)?;
        let source = entry.path().map_err(release_error)?.into_owned();
        let Some(relative) = stripped_path(&source, strip_components)? else {
            continue;
        };
        let destination = target.join(relative);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            fs::create_dir_all(destination)?;
        } else if entry_type.is_file() {
            if extracted_files >= max_extracted_files {
                return Err(Error::Message(
                    "release TAR exceeds max_extracted_files".to_owned(),
                ));
            }
            let remaining = max_extracted_bytes.saturating_sub(extracted_bytes);
            if entry.size() > remaining {
                return Err(Error::Message(
                    "release TAR exceeds max_extracted_bytes".to_owned(),
                ));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = File::create(destination)?;
            let copied = io::copy(
                &mut entry.by_ref().take(remaining.saturating_add(1)),
                &mut output,
            )?;
            if copied > remaining {
                return Err(Error::Message(
                    "release TAR exceeds max_extracted_bytes".to_owned(),
                ));
            }
            output.flush()?;
            extracted_bytes += copied;
            extracted_files += 1;
        } else {
            return Err(Error::Message(format!(
                "release TAR contains unsupported entry `{}`",
                source.display()
            )));
        }
    }
    Ok(extracted_files)
}

fn stripped_path(path: &Path, strip_components: usize) -> Result<Option<PathBuf>> {
    let components = path.components().collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Message(format!(
            "release archive contains unsafe path `{}`",
            path.display()
        )));
    }
    let stripped = components
        .into_iter()
        .skip(strip_components)
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<PathBuf>();
    Ok((!stripped.as_os_str().is_empty()).then_some(stripped))
}

fn load_state(path: &Path) -> Result<ReleaseState> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(Into::into),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ReleaseState::default()),
        Err(error) => Err(error.into()),
    }
}

fn save_state(path: &Path, state: &ReleaseState) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".github-releases-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(&serde_json::to_vec_pretty(state)?)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    temporary.persist(path).map_err(|error| {
        Error::Message(format!(
            "failed to save GitHub release state {}: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn release_error(error: impl std::fmt::Display) -> Error {
    Error::Message(format!("GitHub release operation failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    #[test]
    fn asset_regex_supports_anchored_versions_platforms_and_extensions() {
        let asset_regex =
            Regex::new(r"^tool-[0-9]+\.[0-9]+\.[0-9]+-windows\.zip$").expect("valid asset regex");

        assert!(asset_regex.is_match("tool-1.2.3-windows.zip"));
        assert!(!asset_regex.is_match("prefix-tool-1.2.3-windows.zip"));
        assert!(!asset_regex.is_match("tool-1.2.3-linux.zip"));
        assert!(!asset_regex.is_match("tool-1x2x3-windows.zip"));
    }

    #[test]
    fn disabled_monitor_probe_reads_installed_tag_without_network_access() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state_path = temporary.path().join("github-releases.json");
        let mut state = ReleaseState::default();
        state
            .releases
            .insert("example".to_owned(), "v1.2.3".to_owned());
        save_state(&state_path, &state).expect("save release state");
        let github = GithubSettings {
            poll_interval_secs: 300,
            encrypted_api_key: None,
        };
        let monitors = [GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/repository".to_owned(),
            asset_regex: r"^example\.zip$".to_owned(),
            target_directory: temporary.path().join("installed"),
            format: ReleaseAssetFormat::Zip,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: false,
        }];

        let statuses = probe_monitors(&monitors, &github, &NetworkSettings::default(), &state_path)
            .expect("probe disabled monitor");

        assert_eq!(
            statuses,
            [MonitorStatus {
                name: "example".to_owned(),
                installed_tag: Some("v1.2.3".to_owned()),
                latest_tag: None,
                asset: None,
                error: None,
            }]
        );
    }

    #[test]
    fn archive_paths_are_stripped_without_allowing_traversal() {
        assert_eq!(
            stripped_path(Path::new("package/bin/tool.exe"), 1).expect("safe path"),
            Some(PathBuf::from("bin/tool.exe"))
        );
        assert!(stripped_path(Path::new("../tool.exe"), 0).is_err());
    }

    #[test]
    fn zip_and_tar_gz_extract_into_staging_with_explicit_component_stripping() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let zip_path = temporary.path().join("release.zip");
        let zip_file = File::create(&zip_path).expect("ZIP file");
        let mut zip = zip::ZipWriter::new(zip_file);
        zip.start_file(
            "package/bin/tool.exe",
            zip::write::SimpleFileOptions::default(),
        )
        .expect("ZIP entry");
        zip.write_all(b"zip binary").expect("ZIP contents");
        zip.finish().expect("finish ZIP");
        let zip_target = temporary.path().join("zip-target");
        fs::create_dir(&zip_target).expect("ZIP target");
        assert_eq!(
            extract_zip(&zip_path, &zip_target, 1, 1_024, 10).expect("extract ZIP"),
            1
        );
        assert_eq!(
            fs::read(zip_target.join("bin/tool.exe")).expect("ZIP output"),
            b"zip binary"
        );

        let tar_path = temporary.path().join("release.tar.gz");
        let tar_file = File::create(&tar_path).expect("TAR.GZ file");
        let encoder = GzEncoder::new(tar_file, Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(10);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "package/bin/tool", &b"tar binary"[..])
            .expect("TAR entry");
        tar.into_inner()
            .expect("finish TAR")
            .finish()
            .expect("finish GZIP");
        let tar_target = temporary.path().join("tar-target");
        fs::create_dir(&tar_target).expect("TAR target");
        assert_eq!(
            extract_tar_gz(&tar_path, &tar_target, 1, 1_024, 10).expect("extract TAR.GZ"),
            1
        );
        assert_eq!(
            fs::read(tar_target.join("bin/tool")).expect("TAR output"),
            b"tar binary"
        );
    }
}
