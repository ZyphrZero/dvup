use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use crate::{
    command,
    config::{Config, Tool},
    error::{Error, Result},
    job::CommandSpec,
};

#[derive(Clone, Debug)]
pub(crate) struct ExecutableCandidate {
    pub(crate) path: PathBuf,
    pub(crate) source: &'static str,
    pub(crate) version: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ExecutableDiagnosis {
    pub(crate) program: String,
    pub(crate) probe_args: Vec<String>,
    pub(crate) candidates: Vec<ExecutableCandidate>,
}

const MAX_CONCURRENT_DIAGNOSES: usize = 8;
const MAX_CONCURRENT_VERSION_PROBES: usize = 8;

#[derive(Clone, Debug)]
struct VersionProbeTask {
    key: String,
    path: PathBuf,
    args: Vec<String>,
}

impl ExecutableDiagnosis {
    pub(crate) fn has_conflict(&self) -> bool {
        self.candidates.len() > 1
    }

    pub(crate) fn versions_differ(&self) -> bool {
        self.candidates
            .iter()
            .filter_map(|candidate| candidate.version.as_deref())
            .collect::<HashSet<_>>()
            .len()
            > 1
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolDiagnosis {
    pub(crate) name: String,
    pub(crate) supported: bool,
    pub(crate) target: ExecutableDiagnosis,
    pub(crate) updater: Option<ExecutableDiagnosis>,
}

impl ToolDiagnosis {
    pub(crate) fn has_conflict(&self) -> bool {
        self.target.has_conflict()
            || self
                .updater
                .as_ref()
                .is_some_and(ExecutableDiagnosis::has_conflict)
    }
}

/// Diagnoses configured tools without changing PATH, installations, or config.
pub fn run(config: &Config, working_directory: &Path, selected_tool: Option<&str>) -> Result<u8> {
    let diagnoses = diagnose(config, working_directory, selected_tool)?;
    let report = render_report(&diagnoses);
    print!("{report}");
    Ok(u8::from(diagnoses.iter().any(ToolDiagnosis::has_conflict)))
}

/// Returns structured, read-only diagnostics for CLI and TUI consumers.
pub(crate) fn diagnose(
    config: &Config,
    working_directory: &Path,
    selected_tool: Option<&str>,
) -> Result<Vec<ToolDiagnosis>> {
    let tools = match selected_tool {
        Some(name) => vec![(
            name.to_owned(),
            config
                .tools
                .get(name)
                .cloned()
                .ok_or_else(|| Error::ToolNotFound(name.to_owned()))?,
        )],
        None => config
            .tools
            .iter()
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect(),
    };
    let path_directories = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let extensions = executable_extensions();
    let mut diagnoses = diagnose_tools(&tools, working_directory, &path_directories, &extensions);
    resolve_diagnosis_versions(&mut diagnoses, working_directory);
    Ok(diagnoses)
}

fn diagnose_tools(
    tools: &[(String, Tool)],
    working_directory: &Path,
    path_directories: &[PathBuf],
    extensions: &[String],
) -> Vec<ToolDiagnosis> {
    if tools.is_empty() {
        return Vec::new();
    }
    let worker_count = tools.len().min(MAX_CONCURRENT_DIAGNOSES);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some((name, tool)) = tools.get(index) else {
                        break;
                    };
                    let mut cache = HashMap::new();
                    let diagnosis = diagnose_tool(
                        name.clone(),
                        tool,
                        working_directory,
                        path_directories,
                        extensions,
                        &mut cache,
                    );
                    if tx.send((index, diagnosis)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut ordered = vec![None; tools.len()];
        for (index, diagnosis) in rx {
            ordered[index] = Some(diagnosis);
        }
        ordered.into_iter().flatten().collect()
    })
}

fn diagnose_tool(
    name: String,
    tool: &Tool,
    working_directory: &Path,
    path_directories: &[PathBuf],
    extensions: &[String],
    cache: &mut HashMap<String, ExecutableDiagnosis>,
) -> ToolDiagnosis {
    let supported = tool.supports_current_platform();
    if !supported {
        return ToolDiagnosis {
            name,
            supported,
            target: ExecutableDiagnosis {
                program: tool.probe.program.clone(),
                probe_args: tool.probe.args.clone(),
                candidates: Vec::new(),
            },
            updater: None,
        };
    }
    let target = diagnose_executable_cached(
        &tool.probe.program,
        &tool.probe.args,
        working_directory,
        path_directories,
        extensions,
        cache,
    );
    let updater_program = executable_name(&tool.program);
    let target_name = executable_name(&tool.probe.program);
    let updater =
        (updater_program != target_name && !is_shell_wrapper(&updater_program)).then(|| {
            diagnose_executable_cached(
                &tool.program,
                &["--version".to_owned()],
                working_directory,
                path_directories,
                extensions,
                cache,
            )
        });
    ToolDiagnosis {
        name,
        supported,
        target,
        updater,
    }
}

fn diagnose_executable_cached(
    program: &str,
    probe_args: &[String],
    working_directory: &Path,
    path_directories: &[PathBuf],
    extensions: &[String],
    cache: &mut HashMap<String, ExecutableDiagnosis>,
) -> ExecutableDiagnosis {
    let key = format!(
        "{}\0{}",
        program.to_ascii_lowercase(),
        probe_args.join("\0")
    );
    if let Some(diagnosis) = cache.get(&key) {
        return diagnosis.clone();
    }
    let candidates = collapse_installation_families(
        program,
        executable_candidates(program, working_directory, path_directories, extensions),
    )
    .into_iter()
    .map(|path| ExecutableCandidate {
        source: installation_source(&path),
        version: None,
        path,
    })
    .collect();
    let diagnosis = ExecutableDiagnosis {
        program: program.to_owned(),
        probe_args: probe_args.to_vec(),
        candidates,
    };
    cache.insert(key, diagnosis.clone());
    diagnosis
}

fn resolve_diagnosis_versions(diagnoses: &mut [ToolDiagnosis], working_directory: &Path) {
    resolve_diagnosis_versions_with(diagnoses, working_directory, &probe_version);
}

fn resolve_diagnosis_versions_with<F>(
    diagnoses: &mut [ToolDiagnosis],
    working_directory: &Path,
    probe: &F,
) where
    F: Fn(&Path, &[String], &Path) -> Option<String> + Sync,
{
    let mut tasks = Vec::new();
    let mut seen = HashSet::new();
    for diagnosis in diagnoses.iter() {
        collect_version_probe_tasks(&diagnosis.target, &mut tasks, &mut seen);
        if let Some(updater) = &diagnosis.updater {
            collect_version_probe_tasks(updater, &mut tasks, &mut seen);
        }
    }

    let versions = run_version_probe_tasks(&tasks, working_directory, probe);
    for diagnosis in diagnoses {
        apply_probe_versions(&mut diagnosis.target, &versions);
        if let Some(updater) = &mut diagnosis.updater {
            apply_probe_versions(updater, &versions);
        }
    }
}

fn collect_version_probe_tasks(
    diagnosis: &ExecutableDiagnosis,
    tasks: &mut Vec<VersionProbeTask>,
    seen: &mut HashSet<String>,
) {
    for candidate in &diagnosis.candidates {
        let key = version_probe_key(&candidate.path, &diagnosis.probe_args);
        if seen.insert(key.clone()) {
            tasks.push(VersionProbeTask {
                key,
                path: candidate.path.clone(),
                args: diagnosis.probe_args.clone(),
            });
        }
    }
}

fn run_version_probe_tasks<F>(
    tasks: &[VersionProbeTask],
    working_directory: &Path,
    probe: &F,
) -> HashMap<String, Option<String>>
where
    F: Fn(&Path, &[String], &Path) -> Option<String> + Sync,
{
    if tasks.is_empty() {
        return HashMap::new();
    }
    let worker_count = tasks.len().min(MAX_CONCURRENT_VERSION_PROBES);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(task) = tasks.get(index) else {
                        break;
                    };
                    let version = probe(&task.path, &task.args, working_directory);
                    if tx.send((task.key.clone(), version)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        rx.into_iter().collect()
    })
}

fn apply_probe_versions(
    diagnosis: &mut ExecutableDiagnosis,
    versions: &HashMap<String, Option<String>>,
) {
    for candidate in &mut diagnosis.candidates {
        let key = version_probe_key(&candidate.path, &diagnosis.probe_args);
        candidate.version = versions.get(&key).cloned().flatten();
    }
}

fn version_probe_key(path: &Path, args: &[String]) -> String {
    format!("{}\0{}", path_key(path), args.join("\0"))
}

fn is_shell_wrapper(program: &str) -> bool {
    matches!(
        program,
        "bash" | "sh" | "powershell" | "pwsh" | "invoke-expression"
    )
}

fn deduplicate_candidates(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen_targets = HashSet::new();
    let mut seen_locations = HashSet::new();
    for candidate in paths {
        if !is_executable_file(&candidate) {
            continue;
        }
        let canonical = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        let location = candidate.parent().unwrap_or(&candidate);
        let canonical_location =
            fs::canonicalize(location).unwrap_or_else(|_| location.to_path_buf());
        if !seen_locations.insert(path_key(&canonical_location)) {
            continue;
        }
        if seen_targets.insert(path_key(&canonical)) {
            candidates.push(candidate);
        }
    }
    candidates
}

fn collapse_installation_families(program: &str, candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    let current_executable = env::current_exe().ok();
    collapse_installation_families_with_current(program, candidates, current_executable.as_deref())
}

fn collapse_installation_families_with_current(
    program: &str,
    candidates: Vec<PathBuf>,
    current_executable: Option<&Path>,
) -> Vec<PathBuf> {
    let mut seen_families = HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| {
            installation_family_key(program, candidate, current_executable)
                .is_none_or(|family| seen_families.insert(family))
        })
        .collect()
}

fn installation_family_key(
    program: &str,
    candidate: &Path,
    current_executable: Option<&Path>,
) -> Option<String> {
    cargo_build_family_key(program, candidate, current_executable)
        .or_else(|| rustup_family_key(program, candidate))
}

fn cargo_build_family_key(
    program: &str,
    candidate: &Path,
    current_executable: Option<&Path>,
) -> Option<String> {
    let current_executable = current_executable?;
    let current_name = current_executable.file_name()?.to_string_lossy();
    if executable_name(program) != executable_name(&current_name) {
        return None;
    }
    let current =
        fs::canonicalize(current_executable).unwrap_or_else(|_| current_executable.to_path_buf());
    let candidate = fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    let profile_directory = current.parent()?;
    let is_current = path_key(&candidate) == path_key(&current);
    let is_profile_deps = candidate.parent().is_some_and(|parent| {
        parent
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("deps"))
            && parent
                .parent()
                .is_some_and(|profile| path_key(profile) == path_key(profile_directory))
    }) && candidate
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(&current_name));
    (is_current || is_profile_deps).then(|| format!("cargo-build:{}", path_key(profile_directory)))
}

fn rustup_family_key(program: &str, candidate: &Path) -> Option<String> {
    let candidate_name = candidate.file_name()?.to_string_lossy();
    let executable = executable_name(program);
    if executable_name(&candidate_name) != executable {
        return None;
    }
    let normalized = candidate
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let root = normalized
        .split_once("/.cargo/bin/")
        .map(|(root, _)| root)
        .or_else(|| {
            normalized
                .split_once("/.rustup/toolchains/")
                .map(|(root, _)| root)
        })?;
    Some(format!("rustup:{root}:{executable}"))
}

fn executable_candidates(
    program: &str,
    working_directory: &Path,
    path_directories: &[PathBuf],
    extensions: &[String],
) -> Vec<PathBuf> {
    let path = Path::new(program);
    let has_directory = path.is_absolute() || path.components().count() > 1;
    let bases = if has_directory {
        vec![if path.is_absolute() {
            path.to_path_buf()
        } else {
            working_directory.join(path)
        }]
    } else {
        let mut directories = Vec::new();
        #[cfg(windows)]
        directories.push(working_directory.to_path_buf());
        directories.extend(path_directories.iter().cloned());
        directories
            .into_iter()
            .map(|directory| directory.join(path))
            .collect()
    };
    let paths = bases.into_iter().flat_map(|base| {
        if base.extension().is_some() {
            vec![base]
        } else {
            extensions
                .iter()
                .map(|extension| {
                    if extension.is_empty() {
                        base.clone()
                    } else {
                        PathBuf::from(format!("{}{}", base.to_string_lossy(), extension))
                    }
                })
                .collect()
        }
    });
    deduplicate_candidates(paths)
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        value.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        value
    }
}

#[cfg(windows)]
fn executable_extensions() -> Vec<String> {
    let mut extensions = match env::var_os("PATHEXT") {
        Some(value) => value
            .to_string_lossy()
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| extension.to_ascii_lowercase())
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };
    // Commands are executed through PowerShell on Windows, where a .ps1
    // launcher takes precedence over application launchers in the same PATH
    // directory.
    extensions.retain(|extension| extension != ".ps1");
    extensions.insert(0, ".ps1".to_owned());
    extensions.push(String::new());
    extensions
}

#[cfg(not(windows))]
fn executable_extensions() -> Vec<String> {
    vec![String::new()]
}

fn executable_name(program: &str) -> String {
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

fn probe_version(path: &Path, args: &[String], working_directory: &Path) -> Option<String> {
    let spec = CommandSpec {
        program: path.to_string_lossy().into_owned(),
        args: args.to_vec(),
        working_directory: working_directory.to_path_buf(),
    };
    let result = if probe_uses_direct_execution(path) {
        command::run_direct(&spec)
    } else {
        command::run(&spec)
    }
    .ok()?;
    result
        .status
        .success()
        .then(|| compact_version(&result.stdout, &result.stderr))
        .flatten()
}

fn probe_uses_direct_execution(path: &Path) -> bool {
    #[cfg(windows)]
    {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("com")
            })
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        true
    }
}

fn compact_version(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    let mut output = String::from_utf8_lossy(stdout).into_owned();
    output.push('\n');
    output.push_str(&String::from_utf8_lossy(stderr));
    let cleaned = strip_ansi(&output).replace('\r', "\n");
    let lines = cleaned
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    for token in lines.iter().flat_map(|line| line.split_whitespace()) {
        let candidate = token.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | '+' | '_')
        });
        let numeric = candidate
            .strip_prefix('v')
            .or_else(|| candidate.strip_prefix('V'))
            .unwrap_or(candidate);
        if numeric.contains('.')
            && numeric.chars().any(|character| character.is_ascii_digit())
            && numeric.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | '_')
            })
        {
            return Some(candidate.to_owned());
        }
    }
    lines.first().map(|line| truncate(line, 48))
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::new();
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        if characters.next_if_eq(&'[').is_some() {
            for character in characters.by_ref() {
                if ('@'..='~').contains(&character) {
                    break;
                }
            }
        }
    }
    output
}

fn truncate(value: &str, max_characters: usize) -> String {
    if value.chars().count() <= max_characters {
        return value.to_owned();
    }
    let mut truncated = value
        .chars()
        .take(max_characters.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn installation_source(path: &Path) -> &'static str {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if is_current_cargo_build_path(path) {
        "cargo-build"
    } else if normalized.contains("/.cargo/bin/") {
        "rustup/cargo"
    } else if normalized.contains("/.bun/bin/") {
        "bun"
    } else if normalized.contains("/scoop/shims/") {
        "scoop"
    } else if normalized.contains("homebrew") || normalized.contains("linuxbrew") {
        "homebrew"
    } else if normalized.contains("/node_modules/") || normalized.contains("/appdata/roaming/npm/")
    {
        "npm"
    } else if normalized.contains("/.local/bin/") {
        "user-local"
    } else if normalized.contains("/program files/")
        || normalized.starts_with("/usr/bin/")
        || normalized.starts_with("/bin/")
    {
        "system"
    } else {
        "PATH"
    }
}

fn is_current_cargo_build_path(path: &Path) -> bool {
    let Some(current) = env::current_exe().ok() else {
        return false;
    };
    let Some(profile_directory) = current.parent() else {
        return false;
    };
    if !profile_directory.join("deps").is_dir() {
        return false;
    }
    let Some(program) = current.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    cargo_build_family_key(program, path, Some(&current)).is_some()
}

fn render_report(diagnoses: &[ToolDiagnosis]) -> String {
    let mut output = String::from("dvup doctor — installation conflict diagnostics\n\n");
    let mut conflicts = 0;
    let mut missing = 0;
    let mut unsupported = 0;
    for diagnosis in diagnoses {
        if !diagnosis.supported {
            unsupported += 1;
            output.push_str(&format!(
                "[SKIP] {}: unsupported on {}\n\n",
                diagnosis.name,
                std::env::consts::OS
            ));
            continue;
        }
        if diagnosis.has_conflict() {
            conflicts += 1;
            output.push_str(&format!("[WARN] {}\n", diagnosis.name));
        } else if diagnosis.target.candidates.is_empty() {
            missing += 1;
            output.push_str(&format!("[INFO] {}\n", diagnosis.name));
        } else {
            output.push_str(&format!("[OK] {}\n", diagnosis.name));
        }
        render_executable(&mut output, "command", &diagnosis.target);
        if let Some(updater) = &diagnosis.updater {
            render_executable(&mut output, "updater", updater);
        }
        if diagnosis.target.versions_differ()
            || diagnosis
                .updater
                .as_ref()
                .is_some_and(ExecutableDiagnosis::versions_differ)
        {
            output.push_str("  conflict: PATH candidates report different versions\n");
        } else if diagnosis.has_conflict() {
            output.push_str("  conflict: multiple installations are visible in PATH\n");
        }
        if diagnosis.has_conflict() {
            output.push_str(
                "  fix: remove the stale installation from PATH or move the intended one first\n",
            );
        }
        output.push('\n');
    }
    output.push_str(&format!(
        "SUMMARY: {} tool(s), {conflicts} conflict(s), {missing} not found, {unsupported} unsupported\n",
        diagnoses.len()
    ));
    output
}

fn render_executable(output: &mut String, label: &str, diagnosis: &ExecutableDiagnosis) {
    let Some((active, shadowed)) = diagnosis.candidates.split_first() else {
        output.push_str(&format!("  {label}: {} — not found\n", diagnosis.program));
        return;
    };
    output.push_str(&format!(
        "  {label}: {}\n  active: {}  [{}]  version {}\n",
        diagnosis.program,
        active.path.display(),
        active.source,
        active.version.as_deref().unwrap_or("unknown")
    ));
    for candidate in shadowed {
        output.push_str(&format!(
            "  shadowed: {}  [{}]  version {}\n",
            candidate.path.display(),
            candidate.source,
            candidate.version.as_deref().unwrap_or("unknown")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_executable(path: &Path) {
        fs::write(path, b"test executable").expect("write executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = path.metadata().expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("executable permissions");
        }
    }

    #[test]
    fn finds_path_candidates_in_effective_order() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir_all(&first).expect("first PATH directory");
        fs::create_dir_all(&second).expect("second PATH directory");
        #[cfg(windows)]
        let (file_name, extensions) = ("example.cmd", vec![".cmd".to_owned()]);
        #[cfg(not(windows))]
        let (file_name, extensions) = ("example", vec![String::new()]);
        create_executable(&first.join(file_name));
        create_executable(&second.join(file_name));

        let candidates = executable_candidates(
            "example",
            temporary.path(),
            &[first.clone(), second.clone()],
            &extensions,
        );

        assert_eq!(candidates, [first.join(file_name), second.join(file_name)]);
    }

    #[test]
    fn treats_shell_launchers_in_one_directory_as_one_installation() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir_all(&first).expect("first PATH directory");
        fs::create_dir_all(&second).expect("second PATH directory");
        create_executable(&first.join("example.primary"));
        create_executable(&first.join("example.secondary"));
        create_executable(&second.join("example.primary"));

        let candidates = executable_candidates(
            "example",
            temporary.path(),
            &[first.clone(), second.clone()],
            &[".primary".to_owned(), ".secondary".to_owned()],
        );

        assert_eq!(
            candidates,
            [
                first.join("example.primary"),
                second.join("example.primary")
            ]
        );
    }

    #[test]
    fn groups_current_cargo_build_and_deps_artifacts_as_one_installation() {
        let current = PathBuf::from("workspace/target/debug/dvup.exe");
        let installed = PathBuf::from("home/.cargo/bin/dvup.exe");
        let candidates = vec![
            current.clone(),
            PathBuf::from("workspace/target/debug/deps/dvup.exe"),
            installed.clone(),
        ];

        let grouped =
            collapse_installation_families_with_current("dvup", candidates, Some(&current));

        assert_eq!(grouped, [current, installed]);
    }

    #[test]
    fn groups_rustup_proxy_and_managed_toolchain_as_one_installation() {
        let proxy = PathBuf::from("home/.cargo/bin/cargo.exe");
        let candidates = vec![
            proxy.clone(),
            PathBuf::from("home/.rustup/toolchains/stable/bin/cargo.exe"),
        ];

        let grouped = collapse_installation_families_with_current("cargo", candidates, None);

        assert_eq!(grouped, [proxy]);
    }

    #[test]
    fn keeps_unrelated_same_version_installations_separate() {
        let first = PathBuf::from("manager-a/bin/uv.exe");
        let second = PathBuf::from("home/.local/bin/uv.exe");

        let grouped = collapse_installation_families_with_current(
            "uv",
            vec![first.clone(), second.clone()],
            None,
        );

        assert_eq!(grouped, [first, second]);
    }

    #[cfg(windows)]
    #[test]
    fn checks_powershell_launcher_before_pathext_applications() {
        assert_eq!(
            executable_extensions().first().map(String::as_str),
            Some(".ps1")
        );
    }

    #[cfg(windows)]
    #[test]
    fn runs_native_windows_candidates_without_starting_powershell() {
        assert!(probe_uses_direct_execution(Path::new("tool.exe")));
        assert!(probe_uses_direct_execution(Path::new("tool.COM")));
        assert!(!probe_uses_direct_execution(Path::new("tool.ps1")));
        assert!(!probe_uses_direct_execution(Path::new("tool.cmd")));
    }

    #[test]
    fn probes_versions_concurrently_with_a_bounded_worker_count() {
        let mut diagnoses = (0..12)
            .map(|index| ToolDiagnosis {
                name: format!("tool-{index}"),
                supported: true,
                target: ExecutableDiagnosis {
                    program: format!("tool-{index}"),
                    probe_args: vec!["--version".to_owned()],
                    candidates: vec![ExecutableCandidate {
                        path: PathBuf::from(format!("bin/tool-{index}")),
                        source: "PATH",
                        version: None,
                    }],
                },
                updater: None,
            })
            .collect::<Vec<_>>();
        let active = AtomicUsize::new(0);
        let max_active = AtomicUsize::new(0);

        resolve_diagnosis_versions_with(&mut diagnoses, Path::new("."), &|path, _, _| {
            let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
            max_active.fetch_max(now_active, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(30));
            active.fetch_sub(1, Ordering::SeqCst);
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });

        assert!((2..=MAX_CONCURRENT_VERSION_PROBES).contains(&max_active.load(Ordering::SeqCst)));
        for (index, diagnosis) in diagnoses.iter().enumerate() {
            assert_eq!(
                diagnosis.target.candidates[0].version.as_deref(),
                Some(format!("tool-{index}").as_str())
            );
        }
    }

    #[test]
    fn probes_an_identical_candidate_only_once() {
        let executable = ExecutableDiagnosis {
            program: "shared".to_owned(),
            probe_args: vec!["--version".to_owned()],
            candidates: vec![ExecutableCandidate {
                path: PathBuf::from("bin/shared"),
                source: "PATH",
                version: None,
            }],
        };
        let mut diagnoses = [
            ToolDiagnosis {
                name: "first".to_owned(),
                supported: true,
                target: executable.clone(),
                updater: None,
            },
            ToolDiagnosis {
                name: "second".to_owned(),
                supported: true,
                target: executable,
                updater: None,
            },
        ];
        let calls = AtomicUsize::new(0);

        resolve_diagnosis_versions_with(&mut diagnoses, Path::new("."), &|_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Some("1.0.0".to_owned())
        });

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(diagnoses.iter().all(|diagnosis| {
            diagnosis.target.candidates[0].version.as_deref() == Some("1.0.0")
        }));
    }

    #[test]
    fn concurrent_diagnosis_preserves_requested_tool_order() {
        let excluded_platform = match std::env::consts::OS {
            "windows" => "linux",
            _ => "windows",
        };
        let tools = ["zeta", "alpha", "middle"]
            .into_iter()
            .map(|name| {
                let mut tool = Tool::custom(name, name.to_owned(), vec!["update".to_owned()]);
                tool.platforms = vec![excluded_platform.to_owned()];
                (name.to_owned(), tool)
            })
            .collect::<Vec<_>>();

        let diagnoses = diagnose_tools(&tools, Path::new("."), &[], &[String::new()]);

        assert_eq!(
            diagnoses
                .iter()
                .map(|diagnosis| diagnosis.name.as_str())
                .collect::<Vec<_>>(),
            ["zeta", "alpha", "middle"]
        );
        assert!(diagnoses.iter().all(|diagnosis| !diagnosis.supported));
    }

    #[test]
    fn report_calls_out_shadowed_different_versions() {
        let diagnosis = ToolDiagnosis {
            name: "codex".to_owned(),
            supported: true,
            target: ExecutableDiagnosis {
                program: "codex".to_owned(),
                probe_args: vec!["--version".to_owned()],
                candidates: vec![
                    ExecutableCandidate {
                        path: PathBuf::from("first/codex"),
                        source: "npm",
                        version: Some("1.0.0".to_owned()),
                    },
                    ExecutableCandidate {
                        path: PathBuf::from("second/codex"),
                        source: "system",
                        version: Some("0.9.0".to_owned()),
                    },
                ],
            },
            updater: None,
        };

        let report = render_report(&[diagnosis]);

        assert!(report.contains("[WARN] codex"));
        assert!(report.contains("active: first/codex"));
        assert!(report.contains("shadowed: second/codex"));
        assert!(report.contains("different versions"));
        assert!(report.contains("1 conflict(s)"));
    }

    #[test]
    fn compact_version_ignores_labels_and_ansi() {
        assert_eq!(
            compact_version(b"\x1b[32mHomebrew 4.5.8\x1b[0m\n", b""),
            Some("4.5.8".to_owned())
        );
    }
}
