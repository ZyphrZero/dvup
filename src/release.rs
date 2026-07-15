use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
use std::process::Command;

use crate::{
    config::{
        AssetArchitecture, AssetOperatingSystem, DEFAULT_MAX_DOWNLOAD_BYTES,
        DEFAULT_MAX_EXTRACTED_BYTES, DEFAULT_MAX_EXTRACTED_FILES, GithubReleaseMonitor,
        LatestVersionSource, ReleaseAssetFormat,
    },
    credential,
    error::{Error, Result},
    settings::{GithubSettings, NetworkSettings},
    version,
};

const USER_AGENT: &str = concat!("dvup/", env!("CARGO_PKG_VERSION"));
const MAX_GITHUB_RELEASE_METADATA_BYTES: u64 = 4 * 1024 * 1024;

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
        failure: MonitorFailure,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MonitorFailureStage {
    Metadata,
    LocalInspection,
    Download,
    Installation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MonitorFailure {
    stage: MonitorFailureStage,
    detail: String,
}

impl MonitorFailure {
    pub(crate) fn new(stage: MonitorFailureStage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
        }
    }

    pub(crate) fn stage(&self) -> MonitorFailureStage {
        self.stage
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MonitorStatus {
    pub(crate) name: String,
    pub(crate) installed_tag: Option<String>,
    pub(crate) latest_tag: Option<String>,
    pub(crate) asset: Option<String>,
    pub(crate) failure: Option<MonitorFailure>,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct GithubAsset {
    pub(crate) name: String,
    pub(crate) url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GithubReleaseInfo {
    pub(crate) tag: String,
    pub(crate) assets: Vec<GithubAsset>,
}

pub(crate) fn normalize_github_repository(input: &str) -> Result<String> {
    let input = input.trim().trim_end_matches('/');
    let repository = input
        .strip_prefix("https://github.com/")
        .or_else(|| input.strip_prefix("http://github.com/"))
        .unwrap_or(input);
    let repository = repository
        .strip_suffix(".git")
        .unwrap_or(repository)
        .to_owned();
    LatestVersionSource::GithubRelease {
        repository: repository.clone(),
    }
    .validate("GitHub repository")?;
    Ok(repository)
}

pub(crate) fn fetch_latest_release(
    repository: &str,
    github: &GithubSettings,
    network: &NetworkSettings,
) -> Result<GithubReleaseInfo> {
    let repository = normalize_github_repository(repository)?;
    github.validate()?;
    let api_key = credential::github_api_key(github.encrypted_api_key.as_deref())?;
    let agent = version::network_agent(network, version::NetworkRequestPolicy::Metadata)?;
    let release = fetch_latest_release_with_agent(
        &repository,
        api_key.as_ref().map(|key| key.as_str()),
        &agent,
    )?;
    if release.assets.is_empty() {
        return Err(Error::Message(format!(
            "GitHub repository `{repository}` latest Release has no assets"
        )));
    }
    Ok(GithubReleaseInfo {
        tag: release.tag_name,
        assets: release.assets,
    })
}

pub(crate) fn compatible_release_assets(release: &GithubReleaseInfo) -> Result<Vec<GithubAsset>> {
    let os = AssetOperatingSystem::current().ok_or_else(|| {
        Error::Message(format!(
            "unsupported operating system `{}`",
            std::env::consts::OS
        ))
    })?;
    let arch = AssetArchitecture::current().ok_or_else(|| {
        Error::Message(format!(
            "unsupported CPU architecture `{}`",
            std::env::consts::ARCH
        ))
    })?;
    let mut assets = release
        .assets
        .iter()
        .filter(|asset| {
            let lower = asset.name.to_ascii_lowercase();
            !is_release_metadata_asset(&lower)
                && os.matches_name(&lower)
                && arch.matches_name(&lower)
                && [
                    ReleaseAssetFormat::File,
                    ReleaseAssetFormat::Zip,
                    ReleaseAssetFormat::TarGz,
                    ReleaseAssetFormat::Dmg,
                ]
                .into_iter()
                .any(|format| format.matches_name(&lower))
        })
        .cloned()
        .collect::<Vec<_>>();
    assets.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(assets)
}

fn is_release_metadata_asset(name: &str) -> bool {
    let known_metadata = [
        "checksum",
        "checksums",
        "sha256",
        "sha512",
        ".sha256",
        ".sha512",
        ".minisig",
        ".sig",
        ".asc",
        "sbom",
        "provenance",
        "attestation",
        "source-code",
    ]
    .iter()
    .any(|marker| name.contains(marker));
    let source_package = name
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| matches!(part, "source" | "src"));
    known_metadata || source_package
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
        .then(|| version::network_agent(network, version::NetworkRequestPolicy::Metadata))
        .transpose()?;
    let api_key = enabled
        .then(|| credential::github_api_key(github.encrypted_api_key.as_deref()))
        .transpose()?
        .flatten();
    Ok(monitors
        .iter()
        .map(|monitor| {
            let installed_tag = match installed_monitor_version(monitor, &state) {
                Ok(version) => version,
                Err(error) => {
                    return MonitorStatus {
                        name: monitor.name.clone(),
                        installed_tag: None,
                        latest_tag: None,
                        asset: None,
                        failure: Some(MonitorFailure::new(
                            MonitorFailureStage::LocalInspection,
                            error.to_string(),
                        )),
                    };
                }
            };
            if !monitor.enabled {
                return MonitorStatus {
                    name: monitor.name.clone(),
                    installed_tag,
                    latest_tag: None,
                    asset: None,
                    failure: None,
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
                    failure: None,
                },
                Err(error) => MonitorStatus {
                    name: monitor.name.clone(),
                    installed_tag,
                    latest_tag: None,
                    asset: None,
                    failure: Some(MonitorFailure::new(
                        MonitorFailureStage::Metadata,
                        error.to_string(),
                    )),
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
    let metadata_agent = version::network_agent(network, version::NetworkRequestPolicy::Metadata)?;
    let download_agent =
        version::network_agent(network, version::NetworkRequestPolicy::ReleaseAsset)?;
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
                &metadata_agent,
                &download_agent,
                &state,
            ) {
                Ok(MonitorOutcome::Updated { name, tag, asset }) => {
                    state.releases.insert(name.clone(), tag.clone());
                    changed = true;
                    MonitorOutcome::Updated { name, tag, asset }
                }
                Ok(outcome) => outcome,
                Err(failure) => MonitorOutcome::Failed {
                    name: monitor.name.clone(),
                    failure,
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
    metadata_agent: &ureq::Agent,
    download_agent: &ureq::Agent,
    state: &ReleaseState,
) -> std::result::Result<MonitorOutcome, MonitorFailure> {
    let release = resolve_latest_asset(monitor, api_key, metadata_agent)
        .map_err(|error| MonitorFailure::new(MonitorFailureStage::Metadata, error.to_string()))?;
    if installed_monitor_version(monitor, state)
        .map_err(|error| {
            MonitorFailure::new(MonitorFailureStage::LocalInspection, error.to_string())
        })?
        .as_deref()
        .is_some_and(|installed| release_versions_match(installed, &release.tag))
    {
        return Ok(MonitorOutcome::Current {
            name: monitor.name.clone(),
            tag: release.tag,
        });
    }

    let downloaded = download_asset(monitor, api_key, download_agent, &release.asset)
        .map_err(|error| MonitorFailure::new(MonitorFailureStage::Download, error.to_string()))?;
    install_downloaded_asset(monitor, &release.asset, downloaded).map_err(|error| {
        MonitorFailure::new(MonitorFailureStage::Installation, error.to_string())
    })?;

    Ok(MonitorOutcome::Updated {
        name: monitor.name.clone(),
        tag: release.tag,
        asset: release.asset.name,
    })
}

fn installed_monitor_version(
    monitor: &GithubReleaseMonitor,
    state: &ReleaseState,
) -> Result<Option<String>> {
    match monitor.asset.format {
        ReleaseAssetFormat::Dmg => {
            #[cfg(target_os = "macos")]
            return installed_macos_app_version(&monitor.target_directory);
            #[cfg(not(target_os = "macos"))]
            return Err(Error::Message(
                "macOS application version probing is unavailable on this platform".to_owned(),
            ));
        }
        _ => Ok(state.releases.get(&monitor.name).cloned()),
    }
}

pub(crate) fn release_versions_match(installed: &str, release_tag: &str) -> bool {
    fn without_v_prefix(value: &str) -> &str {
        value
            .strip_prefix('v')
            .or_else(|| value.strip_prefix('V'))
            .unwrap_or(value)
    }

    installed == release_tag || without_v_prefix(installed) == without_v_prefix(release_tag)
}

#[cfg(target_os = "macos")]
fn installed_macos_app_version(app: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(app) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Message(format!(
            "macOS application target is not a real directory: {}",
            app.display()
        )));
    }
    let info = app.join("Contents/Info.plist");
    let short = plist_string(&info, "CFBundleShortVersionString")?;
    if let Some(version) = short {
        return Ok(Some(version));
    }
    plist_string(&info, "CFBundleVersion")?.map_or_else(
        || {
            Err(Error::Message(format!(
                "macOS application has no bundle version: {}",
                app.display()
            )))
        },
        |version| Ok(Some(version)),
    )
}

#[cfg(target_os = "macos")]
fn plist_string(path: &Path, key: &str) -> Result<Option<String>> {
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", &format!("Print :{key}")])
        .arg(path)
        .output()?;
    if output.status.success() {
        let value = String::from_utf8(output.stdout)
            .map_err(|_| Error::Message(format!("Info.plist key `{key}` is not UTF-8")))?;
        let value = value.trim();
        return Ok((!value.is_empty()).then(|| value.to_owned()));
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    if detail.contains("Does Not Exist") || detail.contains("Entry, \":") {
        return Ok(None);
    }
    Err(command_error("read macOS application version", &output))
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
    let release = fetch_latest_release_with_agent(&monitor.repository, api_key, agent)?;
    let asset = select_monitor_asset(&monitor.name, &monitor.asset, release.assets)?;
    Ok(ResolvedRelease {
        tag: release.tag_name,
        asset,
    })
}

fn select_monitor_asset(
    monitor_name: &str,
    selector: &crate::config::AssetSelector,
    assets: Vec<GithubAsset>,
) -> Result<GithubAsset> {
    let mut assets = assets
        .into_iter()
        .filter(|asset| !is_release_metadata_asset(&asset.name.to_ascii_lowercase()))
        .filter(|asset| selector.matches(&asset.name))
        .collect::<Vec<_>>();
    if assets.is_empty() {
        return Err(Error::Message(format!(
            "GitHub release monitor `{}` has no compatible release asset",
            monitor_name
        )));
    }
    if assets.len() > 1 {
        return Err(Error::Message(format!(
            "GitHub release monitor `{}` must revisit its release asset selection; the semantic selector matched {} assets",
            monitor_name,
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
            monitor_name
        )));
    }
    Ok(asset)
}

fn fetch_latest_release_with_agent(
    repository: &str,
    api_key: Option<&str>,
    agent: &ureq::Agent,
) -> Result<GithubRelease> {
    let url = format!("https://api.github.com/repos/{repository}/releases/latest");
    let mut response = github_get(agent, &url, api_key, "application/vnd.github+json")?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_GITHUB_RELEASE_METADATA_BYTES)
        .read_json::<GithubRelease>()
        .map_err(release_error)
}

fn download_asset(
    monitor: &GithubReleaseMonitor,
    api_key: Option<&str>,
    agent: &ureq::Agent,
    asset: &GithubAsset,
) -> Result<tempfile::NamedTempFile> {
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
            .take(DEFAULT_MAX_DOWNLOAD_BYTES.saturating_add(1)),
        &mut downloaded,
    )?;
    if copied > DEFAULT_MAX_DOWNLOAD_BYTES {
        return Err(Error::Message(format!(
            "GitHub release monitor `{}` asset exceeds the fixed download safety limit",
            monitor.name
        )));
    }
    downloaded.flush()?;
    downloaded.as_file().sync_all()?;

    Ok(downloaded)
}

fn install_downloaded_asset(
    monitor: &GithubReleaseMonitor,
    asset: &GithubAsset,
    downloaded: tempfile::NamedTempFile,
) -> Result<()> {
    if monitor.asset.format != ReleaseAssetFormat::Dmg {
        fs::create_dir_all(&monitor.target_directory)?;
    }
    match monitor.asset.format {
        ReleaseAssetFormat::File => {
            install_file(downloaded, &monitor.target_directory, &asset.name)?
        }
        ReleaseAssetFormat::Zip => install_archive(&monitor.target_directory, |staging| {
            extract_zip(
                downloaded.path(),
                staging,
                DEFAULT_MAX_EXTRACTED_BYTES,
                DEFAULT_MAX_EXTRACTED_FILES,
            )
        })?,
        ReleaseAssetFormat::TarGz => install_archive(&monitor.target_directory, |staging| {
            extract_tar_gz(
                downloaded.path(),
                staging,
                DEFAULT_MAX_EXTRACTED_BYTES,
                DEFAULT_MAX_EXTRACTED_FILES,
            )
        })?,
        ReleaseAssetFormat::Dmg => {
            #[cfg(target_os = "macos")]
            install_dmg(
                downloaded.path(),
                &monitor.target_directory,
                DEFAULT_MAX_EXTRACTED_BYTES,
                DEFAULT_MAX_EXTRACTED_FILES,
            )?;
            #[cfg(not(target_os = "macos"))]
            return Err(Error::Message(
                "macOS DMG installation is unavailable on this platform".to_owned(),
            ));
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
struct MountedDmg {
    mount_point: tempfile::TempDir,
    attached: bool,
}

#[cfg(target_os = "macos")]
impl MountedDmg {
    fn attach(image: &Path) -> Result<Self> {
        let mount_point = tempfile::Builder::new().prefix("dvup-dmg-").tempdir()?;
        let output = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-readonly", "-nobrowse", "-mountpoint"])
            .arg(mount_point.path())
            .arg(image)
            .output()?;
        if !output.status.success() {
            let _ = Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force"])
                .arg(mount_point.path())
                .output();
            return Err(command_error("hdiutil attach", &output));
        }
        Ok(Self {
            mount_point,
            attached: true,
        })
    }

    fn path(&self) -> &Path {
        self.mount_point.path()
    }

    fn detach(&mut self) -> Result<()> {
        if !self.attached {
            return Ok(());
        }
        let output = Command::new("/usr/bin/hdiutil")
            .arg("detach")
            .arg(self.mount_point.path())
            .output()?;
        if !output.status.success() {
            return Err(command_error("hdiutil detach", &output));
        }
        self.attached = false;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for MountedDmg {
    fn drop(&mut self) {
        if self.attached {
            let _ = Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force"])
                .arg(self.mount_point.path())
                .output();
        }
    }
}

#[cfg(target_os = "macos")]
fn install_dmg(
    image: &Path,
    target: &Path,
    max_extracted_bytes: u64,
    max_extracted_files: usize,
) -> Result<()> {
    let parent = target.parent().ok_or_else(|| {
        Error::Message(format!("DMG target `{}` has no parent", target.display()))
    })?;
    fs::create_dir_all(parent)?;

    let staging_owner = tempfile::Builder::new()
        .prefix(".dvup-app-")
        .tempdir_in(parent)?;
    let target_name = target.file_name().ok_or_else(|| {
        Error::Message(format!(
            "DMG target `{}` has no file name",
            target.display()
        ))
    })?;
    let staged = staging_owner.path().join(target_name);
    let mut mounted = MountedDmg::attach(image)?;
    let prepare = (|| {
        let source = unique_mounted_app(mounted.path())?;
        validate_app_bundle_limits(&source, max_extracted_bytes, max_extracted_files)?;
        let copy = Command::new("/usr/bin/ditto")
            .arg(&source)
            .arg(&staged)
            .output()?;
        if !copy.status.success() {
            return Err(command_error("ditto", &copy));
        }
        let verify = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict", "--verbose=2"])
            .arg(&staged)
            .output()?;
        if !verify.status.success() {
            return Err(command_error("codesign verification", &verify));
        }
        Ok(())
    })();
    combine_primary_and_cleanup(prepare, mounted.detach(), "unmount DMG")?;

    replace_app_bundle(&staged, target)
}

#[cfg(target_os = "macos")]
fn unique_mounted_app(mount_point: &Path) -> Result<PathBuf> {
    let mut applications = Vec::new();
    for entry in fs::read_dir(mount_point)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir()
            && path.extension().and_then(|extension| extension.to_str()) == Some("app")
        {
            applications.push(path);
        }
    }
    if applications.len() != 1 {
        return Err(Error::Message(format!(
            "DMG must contain exactly one top-level .app bundle, found {}",
            applications.len()
        )));
    }
    Ok(applications.pop().expect("one mounted application"))
}

#[cfg(target_os = "macos")]
fn validate_app_bundle_limits(
    app: &Path,
    max_extracted_bytes: u64,
    max_extracted_files: usize,
) -> Result<()> {
    let mut pending = vec![app.to_path_buf()];
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !file_type.is_file() && !file_type.is_symlink() {
                return Err(Error::Message(format!(
                    "DMG application contains unsupported entry `{}`",
                    entry.path().display()
                )));
            }
            files = files.saturating_add(1);
            if files > max_extracted_files {
                return Err(Error::Message(
                    "DMG application exceeds max_extracted_files".to_owned(),
                ));
            }
            if file_type.is_file() {
                bytes = bytes.saturating_add(entry.metadata()?.len());
                if bytes > max_extracted_bytes {
                    return Err(Error::Message(
                        "DMG application exceeds max_extracted_bytes".to_owned(),
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn replace_app_bundle(staged: &Path, target: &Path) -> Result<()> {
    let parent = target.parent().ok_or_else(|| {
        Error::Message(format!(
            "application target `{}` has no parent",
            target.display()
        ))
    })?;
    let backup_owner = tempfile::Builder::new()
        .prefix(".dvup-app-backup-")
        .tempdir_in(parent)?;
    let backup = backup_owner.keep();
    fs::remove_dir(&backup)?;
    let had_target = match fs::symlink_metadata(target) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if had_target {
        fs::rename(target, &backup)?;
    }
    if let Err(error) = fs::rename(staged, target) {
        if had_target {
            if let Err(rollback_error) = fs::rename(&backup, target) {
                return Err(Error::Message(format!(
                    "failed to install `{}`: {error}; rollback also failed: {rollback_error}; original application remains at `{}`",
                    target.display(),
                    backup.display()
                )));
            }
        }
        return Err(error.into());
    }
    if had_target {
        let _ = remove_path(&backup);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(target_os = "macos")]
fn combine_primary_and_cleanup(
    primary: Result<()>,
    cleanup: Result<()>,
    cleanup_name: &str,
) -> Result<()> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(Error::Message(format!(
            "{primary}; {cleanup_name} also failed: {cleanup}"
        ))),
    }
}

#[cfg(target_os = "macos")]
fn command_error(operation: &str, output: &std::process::Output) -> Error {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Error::Message(if detail.is_empty() {
        format!("{operation} failed with {}", output.status)
    } else {
        format!("{operation} failed: {detail}")
    })
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
    set_installed_file_mode(downloaded.path(), 0o755)?;
    downloaded.persist(&target).map_err(|error| {
        Error::Message(format!("failed to install {}: {}", target.display(), error))
    })?;
    Ok(())
}

fn install_archive(
    target_directory: &Path,
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
    let install_root = archive_install_root(&staging)?;

    let backup_owner = tempfile::Builder::new()
        .prefix(".dvup-backup-")
        .tempdir_in(parent)?;
    let backup = backup_owner.keep();
    fs::remove_dir(&backup)?;
    let had_target = target_directory.exists();
    if had_target {
        fs::rename(target_directory, &backup)?;
    }
    if let Err(error) = fs::rename(&install_root, target_directory) {
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

fn archive_install_root(staging: &Path) -> Result<PathBuf> {
    let mut entries = fs::read_dir(staging)?.collect::<std::result::Result<Vec<_>, _>>()?;
    if entries.len() != 1 {
        return Ok(staging.to_path_buf());
    }
    let entry = entries.pop().expect("one archive top-level entry");
    let file_type = entry.file_type()?;
    if file_type.is_dir() && !file_type.is_symlink() {
        Ok(entry.path())
    } else {
        Ok(staging.to_path_buf())
    }
}

fn extract_zip(
    archive: &Path,
    target: &Path,
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
        let Some(relative) = safe_archive_path(&source)? else {
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
        let mode = entry.unix_mode().unwrap_or(0o755);
        let mut output = File::create(&destination)?;
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
        drop(output);
        set_installed_file_mode(&destination, mode)?;
        extracted_bytes += copied;
        extracted_files += 1;
    }
    Ok(extracted_files)
}

fn extract_tar_gz(
    archive: &Path,
    target: &Path,
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
        let Some(relative) = safe_archive_path(&source)? else {
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
            let mode = entry.header().mode().map_err(release_error)?;
            let mut output = File::create(&destination)?;
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
            drop(output);
            set_installed_file_mode(&destination, mode)?;
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

#[cfg(unix)]
fn set_installed_file_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_installed_file_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn safe_archive_path(path: &Path) -> Result<Option<PathBuf>> {
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
    fn normalizes_supported_github_repository_inputs_without_changing_repositories() {
        assert_eq!(
            normalize_github_repository("https://github.com/owner/repository.git/")
                .expect("GitHub URL"),
            "owner/repository"
        );
        assert_eq!(
            normalize_github_repository("owner/repository").expect("owner/repo"),
            "owner/repository"
        );
        assert!(normalize_github_repository("repository").is_err());
        assert!(normalize_github_repository("owner/repository/extra").is_err());
    }

    #[cfg(target_os = "macos")]
    fn create_signed_test_app(parent: &Path, name: &str, marker: &str) -> PathBuf {
        use std::{os::unix::fs::PermissionsExt, process::Command};

        let app = parent.join(format!("{name}.app"));
        let contents = app.join("Contents");
        let executable = contents.join("MacOS").join(name);
        fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("create executable directory");
        fs::create_dir_all(contents.join("Resources")).expect("create resources directory");
        fs::write(
            contents.join("Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{name}</string>
<key>CFBundleIdentifier</key><string>dev.dvup.test.{}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
</dict></plist>
"#,
                name.to_ascii_lowercase()
            ),
        )
        .expect("write Info.plist");
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("write executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .expect("make executable");
        fs::write(contents.join("Resources/version.txt"), marker).expect("write marker");
        let output = Command::new("/usr/bin/codesign")
            .args(["--force", "--deep", "--sign", "-"])
            .arg(&app)
            .output()
            .expect("run codesign");
        assert!(
            output.status.success(),
            "codesign failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        app
    }

    #[cfg(target_os = "macos")]
    fn create_test_dmg(source: &Path, image: &Path) {
        use std::process::Command;

        let output = Command::new("/usr/bin/hdiutil")
            .args(["create", "-quiet", "-format", "UDZO", "-srcfolder"])
            .arg(source)
            .arg(image)
            .output()
            .expect("create test DMG");
        assert!(
            output.status.success(),
            "hdiutil create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dmg_install_replaces_the_existing_app_with_the_signed_bundle() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let source = temporary.path().join("image-source");
        fs::create_dir(&source).expect("image source");
        create_signed_test_app(&source, "Reqable", "new release");
        let image = temporary.path().join("reqable.dmg");
        create_test_dmg(&source, &image);

        let applications = temporary.path().join("Applications");
        fs::create_dir(&applications).expect("applications directory");
        let target = applications.join("Reqable.app");
        fs::create_dir_all(target.join("Contents/Resources")).expect("old app");
        fs::write(target.join("Contents/Resources/version.txt"), "old release")
            .expect("old marker");

        install_dmg(&image, &target, 10 * 1024 * 1024, 1_000).expect("install DMG app");

        assert_eq!(
            fs::read_to_string(target.join("Contents/Resources/version.txt"))
                .expect("installed marker"),
            "new release"
        );
    }

    #[test]
    fn archive_paths_preserve_layout_without_allowing_traversal() {
        assert_eq!(
            safe_archive_path(Path::new("package/bin/tool.exe")).expect("safe path"),
            Some(PathBuf::from("package/bin/tool.exe"))
        );
        assert!(safe_archive_path(Path::new("../tool.exe")).is_err());
    }

    #[test]
    fn compatible_assets_exclude_metadata_and_incompatible_platforms() {
        let os = crate::config::AssetOperatingSystem::current().expect("supported test OS");
        let arch = crate::config::AssetArchitecture::current().expect("supported test arch");
        let compatible_name = format!(
            "tool-1.0.0-{}-{}.tar.gz",
            os.aliases()[0],
            arch.aliases()[0]
        );
        let incompatible_arch = match arch {
            crate::config::AssetArchitecture::Aarch64 => "amd64",
            crate::config::AssetArchitecture::X86_64 => "arm64",
        };
        let release = GithubReleaseInfo {
            tag: "v1.0.0".to_owned(),
            assets: vec![
                GithubAsset {
                    name: compatible_name.clone(),
                    url: "https://example.invalid/tool".to_owned(),
                },
                GithubAsset {
                    name: format!("{compatible_name}.sha256"),
                    url: "https://example.invalid/checksum".to_owned(),
                },
                GithubAsset {
                    name: format!(
                        "tool-source-{}-{}.tar.gz",
                        os.aliases()[0],
                        arch.aliases()[0]
                    ),
                    url: "https://example.invalid/source".to_owned(),
                },
                GithubAsset {
                    name: format!("tool-1.0.0-{}-{incompatible_arch}.tar.gz", os.aliases()[0]),
                    url: "https://example.invalid/wrong-arch".to_owned(),
                },
            ],
        };

        assert_eq!(
            compatible_release_assets(&release).expect("compatible assets"),
            [GithubAsset {
                name: compatible_name,
                url: "https://example.invalid/tool".to_owned(),
            }]
        );
    }

    #[test]
    fn monitor_asset_selection_rejects_zero_and_multiple_matches() {
        let os = crate::config::AssetOperatingSystem::current().expect("supported test OS");
        let arch = crate::config::AssetArchitecture::current().expect("supported test arch");
        let selector = crate::config::AssetSelector {
            product: "tool".to_owned(),
            os,
            arch,
            format: ReleaseAssetFormat::TarGz,
            variant: None,
        };
        let first = GithubAsset {
            name: format!(
                "tool-1.0.0-{}-{}.tar.gz",
                os.aliases()[0],
                arch.aliases()[0]
            ),
            url: "https://example.invalid/one".to_owned(),
        };
        let second = GithubAsset {
            name: format!(
                "tool-2.0.0-{}-{}.tar.gz",
                os.aliases()[0],
                arch.aliases()[0]
            ),
            url: "https://example.invalid/two".to_owned(),
        };

        let none = select_monitor_asset("tool", &selector, Vec::new())
            .expect_err("zero matches must fail");
        let many = select_monitor_asset("tool", &selector, vec![first, second])
            .expect_err("multiple matches must fail");
        assert!(none.to_string().contains("no compatible release asset"));
        assert!(
            many.to_string()
                .contains("revisit its release asset selection")
        );
    }

    #[test]
    fn archives_drop_one_common_top_level_directory_and_otherwise_preserve_layout() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let zip_path = temporary.path().join("release.zip");
        let zip_file = File::create(&zip_path).expect("ZIP file");
        let mut zip = zip::ZipWriter::new(zip_file);
        zip.start_file(
            "package/bin/tool.exe",
            zip::write::SimpleFileOptions::default().unix_permissions(0o755),
        )
        .expect("ZIP entry");
        zip.write_all(b"zip binary").expect("ZIP contents");
        zip.finish().expect("finish ZIP");
        let zip_target = temporary.path().join("zip-target");
        install_archive(&zip_target, |staging| {
            extract_zip(&zip_path, staging, 1_024, 10)
        })
        .expect("install ZIP");
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
        let mut second_header = tar::Header::new_gnu();
        second_header.set_size(7);
        second_header.set_mode(0o644);
        second_header.set_cksum();
        tar.append_data(&mut second_header, "LICENSE", &b"license"[..])
            .expect("second TAR entry");
        tar.into_inner()
            .expect("finish TAR")
            .finish()
            .expect("finish GZIP");
        let tar_target = temporary.path().join("tar-target");
        install_archive(&tar_target, |staging| {
            extract_tar_gz(&tar_path, staging, 1_024, 10)
        })
        .expect("install TAR.GZ");
        assert_eq!(
            fs::read(tar_target.join("package/bin/tool")).expect("TAR output"),
            b"tar binary"
        );
        assert_eq!(
            fs::read(tar_target.join("LICENSE")).expect("preserved top-level file"),
            b"license"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(zip_target.join("bin/tool.exe"))
                    .expect("ZIP executable metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                fs::metadata(tar_target.join("package/bin/tool"))
                    .expect("TAR executable metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                fs::metadata(tar_target.join("LICENSE"))
                    .expect("TAR data metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn plain_release_files_are_installed_as_user_executables() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let target = temporary.path().join("target");
        fs::create_dir(&target).expect("target directory");
        let mut downloaded =
            tempfile::NamedTempFile::new_in(temporary.path()).expect("downloaded release file");
        downloaded.write_all(b"binary").expect("download contents");

        install_file(downloaded, &target, "tool").expect("install plain release file");

        assert_eq!(
            fs::metadata(target.join("tool"))
                .expect("installed file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}
