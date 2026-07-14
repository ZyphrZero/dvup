use std::{fs, process::Command};

#[test]
fn default_loading_uses_global_custom_manifest_and_ignores_current_directory() {
    let temporary = tempfile::TempDir::new().expect("temp dir");
    let state = temporary.path().join("state");
    let working_directory = temporary.path().join("project");
    fs::create_dir_all(&state).expect("state directory");
    fs::create_dir_all(&working_directory).expect("working directory");
    fs::write(
        state.join("dvup_custom.toml"),
        concat!(
            "[commands.global_only]\n",
            "type = \"custom\"\n",
            "update = [\"global-updater\", \"update\"]\n",
            "probe = [\"global-tool\", \"--version\"]\n",
        ),
    )
    .expect("global custom manifest");
    fs::write(
        working_directory.join(".dvup.toml"),
        concat!(
            "[commands.local_only]\n",
            "type = \"custom\"\n",
            "update = [\"local-updater\", \"update\"]\n",
            "probe = [\"local-tool\", \"--version\"]\n",
        ),
    )
    .expect("current-directory manifest");

    let output = Command::new(env!("CARGO_BIN_EXE_dvup"))
        .current_dir(&working_directory)
        .arg("--state-dir")
        .arg(&state)
        .arg("list")
        .output()
        .expect("run dvup list");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("global_only"), "stdout: {stdout}");
    assert!(!stdout.contains("local_only"), "stdout: {stdout}");
}

#[test]
fn init_without_an_explicit_path_creates_the_global_custom_manifest() {
    let temporary = tempfile::TempDir::new().expect("temp dir");
    let state = temporary.path().join("state");
    let working_directory = temporary.path().join("project");
    fs::create_dir_all(&working_directory).expect("working directory");

    let output = Command::new(env!("CARGO_BIN_EXE_dvup"))
        .current_dir(&working_directory)
        .arg("--state-dir")
        .arg(&state)
        .arg("init")
        .output()
        .expect("run dvup init");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(state.join("dvup_custom.toml").is_file());
    assert!(!working_directory.join(".dvup.toml").exists());
}
