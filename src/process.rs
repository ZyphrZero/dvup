use std::collections::{HashMap, HashSet};

use sysinfo::{Pid, ProcessesToUpdate, Signal, System};

use crate::{
    config::{ProcessAction, ProcessRule},
    job::LockingProcess,
};

/// A running process together with the highest-priority matching rule.
#[derive(Clone, Debug)]
pub struct MatchedProcess {
    pub process: LockingProcess,
    pub action: ProcessAction,
    pub terminate_grace_secs: u64,
}

/// Returns running processes matching the configured rules.
pub fn find_matching_processes(rules: &[ProcessRule]) -> Vec<MatchedProcess> {
    if rules.is_empty() {
        return Vec::new();
    }

    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let protected_pids = current_process_ancestry(&system);
    let mut matches: HashMap<u32, MatchedProcess> = HashMap::new();

    for (pid, process) in system.processes() {
        let pid = pid.as_u32();
        if protected_pids.contains(&pid) {
            continue;
        }
        let name = process.name().to_string_lossy().into_owned();
        let command = process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let executable = process
            .exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();

        for rule in rules {
            if !rule_matches(rule, &name, &command, &executable) {
                continue;
            }
            let candidate = MatchedProcess {
                process: LockingProcess {
                    pid,
                    name: name.clone(),
                    start_time: process.start_time(),
                },
                action: rule.action,
                terminate_grace_secs: rule.terminate_grace_secs,
            };
            match matches.get(&pid) {
                Some(existing)
                    if action_priority(existing.action) >= action_priority(rule.action) => {}
                _ => {
                    matches.insert(pid, candidate);
                }
            }
        }
    }

    let mut matches: Vec<_> = matches.into_values().collect();
    matches.sort_by_key(|matched| matched.process.pid);
    matches
}

fn current_process_ancestry(system: &System) -> HashSet<u32> {
    let mut protected = HashSet::new();
    let mut current = Some(Pid::from_u32(std::process::id()));
    while let Some(pid) = current {
        if !protected.insert(pid.as_u32()) {
            break;
        }
        current = system.process(pid).and_then(sysinfo::Process::parent);
    }
    protected
}

/// Requests graceful termination of the exact process instance.
pub fn request_termination(target: &LockingProcess) -> bool {
    with_exact_process(target, |process| {
        process.kill_with(Signal::Term).unwrap_or(false)
    })
}

/// Forcefully kills the exact process instance if it is still running.
pub fn force_kill(target: &LockingProcess) -> bool {
    with_exact_process(target, sysinfo::Process::kill)
}

/// Returns whether the exact process instance is still running.
pub fn is_alive(target: &LockingProcess) -> bool {
    with_exact_process(target, |_| true)
}

fn with_exact_process(
    target: &LockingProcess,
    operation: impl FnOnce(&sysinfo::Process) -> bool,
) -> bool {
    let pid = Pid::from_u32(target.pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let Some(process) = system.process(pid) else {
        return false;
    };
    if process.start_time() != target.start_time
        || normalize_name(&process.name().to_string_lossy()) != normalize_name(&target.name)
    {
        return false;
    }
    operation(process)
}

fn rule_matches(rule: &ProcessRule, name: &str, command: &str, executable: &str) -> bool {
    if normalize_name(name) != normalize_name(&rule.name) {
        return false;
    }
    rule.command_contains.as_ref().is_none_or(|needle| {
        let needle = normalize_command_fragment(needle);
        normalize_command_fragment(command).contains(&needle)
            || normalize_command_fragment(executable).contains(&needle)
    })
}

fn normalize_command_fragment(value: &str) -> String {
    value.trim().to_lowercase().replace('\\', "/")
}

fn action_priority(action: ProcessAction) -> u8 {
    match action {
        ProcessAction::Wait => 0,
        ProcessAction::Terminate => 1,
        ProcessAction::Fail => 2,
    }
}

fn normalize_name(name: &str) -> String {
    let normalized = name.trim().to_lowercase();
    normalized
        .strip_suffix(".exe")
        .unwrap_or(&normalized)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, command_contains: Option<&str>) -> ProcessRule {
        ProcessRule {
            name: name.to_owned(),
            command_contains: command_contains.map(str::to_owned),
            action: ProcessAction::Terminate,
            terminate_grace_secs: 1,
        }
    }

    #[test]
    fn normalizes_cross_platform_process_names() {
        let cases = [
            ("CODEX.EXE", "codex"),
            (" codex ", "codex"),
            ("node", "node"),
        ];

        for (input, expected) in cases {
            assert_eq!(normalize_name(input), expected);
        }
    }

    #[test]
    fn command_filter_scopes_runtime_processes() {
        let node_rule = rule("node", Some("@openai/codex"));

        assert!(rule_matches(
            &node_rule,
            "node.exe",
            r#"node C:\tools\@openai\codex\bin\codex.js"#,
            r#"C:\node.exe"#
        ));
        assert!(!rule_matches(
            &node_rule,
            "node.exe",
            r#"node C:\editor\typescript-language-server.js"#,
            r#"C:\node.exe"#
        ));
        assert!(!rule_matches(
            &node_rule,
            "python.exe",
            r#"python @openai/codex"#,
            r#"C:\python.exe"#
        ));
        assert!(rule_matches(
            &node_rule,
            "node.exe",
            "",
            r#"C:\tools\@openai\codex\node.exe"#
        ));
    }

    #[test]
    fn fail_rules_have_highest_priority() {
        assert!(action_priority(ProcessAction::Fail) > action_priority(ProcessAction::Terminate));
        assert!(action_priority(ProcessAction::Terminate) > action_priority(ProcessAction::Wait));
    }

    #[test]
    fn matching_never_targets_the_process_that_launched_dvup() {
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let current_pid = Pid::from_u32(std::process::id());
        let parent_pid = system
            .process(current_pid)
            .and_then(sysinfo::Process::parent)
            .expect("test process has a live parent");
        let parent_name = system
            .process(parent_pid)
            .expect("parent process")
            .name()
            .to_string_lossy()
            .into_owned();

        let matches = find_matching_processes(&[rule(&parent_name, None)]);

        assert!(
            matches
                .iter()
                .all(|matched| matched.process.pid != parent_pid.as_u32()),
            "the launcher process must be protected from process policies"
        );
    }
}
