// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;
use tempfile::TempDir;

#[test]
fn status_defaults_depos_root_to_home_dot_depos_and_creates_it() {
    let temp_home = TempDir::new().expect("failed to create temp home");
    let default_root = temp_home.path().join(".depos");

    let output = Command::new(env!("CARGO_BIN_EXE_depos"))
        .arg("status")
        .env("HOME", temp_home.path())
        .output()
        .expect("failed to run depos status");

    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        default_root.is_dir(),
        "expected {} to exist",
        default_root.display()
    );
    assert!(
        output.stdout.is_empty(),
        "unexpected stdout: {:?}",
        output.stdout
    );
}

#[test]
fn status_uses_explicit_depos_root_instead_of_default() {
    let temp_home = TempDir::new().expect("failed to create temp home");
    let explicit_root = temp_home.path().join("custom-depos");
    let default_root = temp_home.path().join(".depos");

    let output = Command::new(env!("CARGO_BIN_EXE_depos"))
        .arg("status")
        .arg("--depos-root")
        .arg(&explicit_root)
        .env("HOME", temp_home.path())
        .output()
        .expect("failed to run depos status");

    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        explicit_root.is_dir(),
        "expected {} to exist",
        explicit_root.display()
    );
    assert!(
        !default_root.exists(),
        "did not expect default root {} to exist",
        default_root.display()
    );
}
