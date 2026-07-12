use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
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
    pub(crate) candidates: Vec<ExecutableCandidate>,
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
    let mut cache = HashMap::new();
    Ok(tools
        .into_iter()
        .map(|(name, tool)| {
            diagnose_tool(
                name,
                &tool,
                working_directory,
                &path_directories,
                &extensions,
                &mut cache,
            )
        })
        .collect())
}

fn diagnose_tool(
    name: String,
    tool: &Tool,
    working_directory: &Path,
    path_directories: &[PathBuf],
    extensions: &[String],
    cache: &mut HashMap<String, ExecutableDiagnosis>,
) -> ToolDiagnosis {
    let target_program = diagnostic_program(&name, tool);
    let target = diagnose_executable_cached(
        &target_program,
        working_directory,
        path_directories,
        extensions,
        cache,
    );
    let updater_program = executable_name(&tool.program);
    let target_name = executable_name(&target_program);
    let updater =
        (updater_program != target_name && !is_shell_wrapper(&updater_program)).then(|| {
            diagnose_executable_cached(
                &tool.program,
                working_directory,
                path_directories,
                extensions,
                cache,
            )
        });
    ToolDiagnosis {
        name,
        supported: tool.supports_current_platform(),
        target,
        updater,
    }
}

fn diagnose_executable_cached(
    program: &str,
    working_directory: &Path,
    path_directories: &[PathBuf],
    extensions: &[String],
    cache: &mut HashMap<String, ExecutableDiagnosis>,
) -> ExecutableDiagnosis {
    let key = program.to_ascii_lowercase();
    if let Some(diagnosis) = cache.get(&key) {
        return diagnosis.clone();
    }
    let candidates =
        executable_candidates(program, working_directory, path_directories, extensions)
            .into_iter()
            .map(|path| ExecutableCandidate {
                source: installation_source(&path),
                version: probe_version(&path, working_directory),
                path,
            })
            .collect();
    let diagnosis = ExecutableDiagnosis {
        program: program.to_owned(),
        candidates,
    };
    cache.insert(key, diagnosis.clone());
    diagnosis
}

fn diagnostic_program(name: &str, tool: &Tool) -> String {
    let updater = executable_name(&tool.program);
    let known_tool = matches!(
        name.to_ascii_lowercase().as_str(),
        "brew" | "bun" | "codex" | "rustup" | "scoop" | "uv"
    );
    let package_manager = matches!(updater.as_str(), "brew" | "bun" | "npm" | "pnpm" | "scoop");
    if known_tool || package_manager {
        name.to_owned()
    } else {
        tool.program.clone()
    }
}

fn is_shell_wrapper(program: &str) -> bool {
    matches!(
        program,
        "bash" | "sh" | "powershell" | "pwsh" | "invoke-expression"
    )
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
    let mut candidates = Vec::new();
    let mut seen_targets = HashSet::new();
    let mut seen_locations = HashSet::new();
    for base in bases {
        let paths = if base.extension().is_some() {
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
        };
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
    }
    candidates
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
    let mut extensions = env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| extension.to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            vec![
                ".com".to_owned(),
                ".exe".to_owned(),
                ".bat".to_owned(),
                ".cmd".to_owned(),
            ]
        });
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

fn probe_version(path: &Path, working_directory: &Path) -> Option<String> {
    let result = command::run(&CommandSpec {
        program: path.to_string_lossy().into_owned(),
        args: vec!["--version".to_owned()],
        working_directory: working_directory.to_path_buf(),
    })
    .ok()?;
    result
        .status
        .success()
        .then(|| compact_version(&result.stdout, &result.stderr))
        .flatten()
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
    if normalized.contains("/.cargo/bin/") {
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
        create_executable(&first.join("example.fallback"));
        create_executable(&second.join("example.primary"));

        let candidates = executable_candidates(
            "example",
            temporary.path(),
            &[first.clone(), second.clone()],
            &[".primary".to_owned(), ".fallback".to_owned()],
        );

        assert_eq!(
            candidates,
            [
                first.join("example.primary"),
                second.join("example.primary")
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn checks_powershell_launcher_before_pathext_applications() {
        assert_eq!(
            executable_extensions().first().map(String::as_str),
            Some(".ps1")
        );
    }

    #[test]
    fn report_calls_out_shadowed_different_versions() {
        let diagnosis = ToolDiagnosis {
            name: "codex".to_owned(),
            supported: true,
            target: ExecutableDiagnosis {
                program: "codex".to_owned(),
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
