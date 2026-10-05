// Released under the MIT License.
// Copyright, 2026, by Samuel Williams.

use std::process::Command;

#[test]
fn lists_discovered_bake_tasks() {
    let output = Command::new(env!("CARGO_BIN_EXE_bake-releases-cargo-project"))
        .arg("--list")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("releases:cargo:release"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("releases:version:patch"));
}

#[test]
fn runs_release_tasks_from_a_consumer_workspace() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "bake-releases-cargo-cli-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"1.2.3\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "// fixture\n").unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/socketry/fixture.git",
            ])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );

    let plan = Command::new(env!("CARGO_BIN_EXE_bake-releases-cargo-project"))
        .args(["releases:cargo:trusted-publishing:plan", "fixture"])
        .env("BAKE_PROJECT_ROOT", &root)
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(plan.status.success());
    assert!(
        String::from_utf8_lossy(&plan.stdout)
            .contains("https://crates.io/api/v1/trusted_publishing/github_configs")
    );

    let bump = Command::new(env!("CARGO_BIN_EXE_bake-releases-cargo-project"))
        .args(["releases:version:bump", "--version", "1.2.4"])
        .env("BAKE_PROJECT_ROOT", &root)
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(bump.status.success());
    assert!(
        std::fs::read_to_string(root.join("Cargo.toml"))
            .unwrap()
            .contains("version = \"1.2.4\"")
    );

    std::fs::remove_dir_all(root).unwrap();
}
