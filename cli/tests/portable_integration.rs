#![cfg(any(target_os = "macos", target_os = "windows"))]

// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use depos::{host_arch, sync_registry, SyncOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const RELEASE_NAMESPACE: &str = "release";

#[test]
fn sync_builds_cargo_package_with_native_portable_backend() {
    let sandbox = Sandbox::new();
    let package_name = "portable_demo";
    let artifact_name = static_library_file_name(package_name);
    let artifact_stage_source = format!("cargo-target/release/{artifact_name}");
    let artifact_store_path = format!("lib/{artifact_name}");
    let repo = sandbox.create_git_repo(
        "upstreams/portable_demo",
        &[
            (
                "Cargo.toml",
                &format!(
                    "[package]\nname = \"{package_name}\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"staticlib\"]\n"
                ),
            ),
            (
                "src/lib.rs",
                "#[no_mangle]\npub extern \"C\" fn portable_demo_add(left: i32, right: i32) -> i32 {\n    left + right\n}\n",
            ),
            (
                "include/portable_demo/demo.h",
                "#pragma once\nint portable_demo_add(int left, int right);\n",
            ),
        ],
    );
    sandbox.write(
        &format!(
            "depofiles/local/{package_name}/{RELEASE_NAMESPACE}/1.0.0/main.DepoFile"
        ),
        &format!(
            "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nBUILD_SYSTEM CARGO\nCARGO_BUILD cargo build --release --target-dir ${{DEPO_BUILD_DIR}}/cargo-target --manifest-path Cargo.toml\nSTAGE_FILE SOURCE include/{package_name}/demo.h include/{package_name}/demo.h\nSTAGE_FILE BUILD {artifact_stage_source} {artifact_store_path}\nTARGET {package_name}::{package_name} STATIC {artifact_store_path} INTERFACE include\n",
            portable_path(&repo)
        ),
    );
    sandbox.write(
        "manifests/portable_demo.cmake",
        "depos_require(portable_demo VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/portable_demo.cmake"),
        executable: None,
    })
    .expect("sync should build portable native package");

    assert_eq!(output.selected.len(), 1);
    assert!(sandbox
        .package_store_path(
            package_name,
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/portable_demo/demo.h"
        )
        .is_file());
    assert!(sandbox
        .package_store_path(
            package_name,
            RELEASE_NAMESPACE,
            "1.0.0",
            &artifact_store_path
        )
        .is_file());
}

#[test]
fn sync_rejects_build_root_scratch_off_linux() {
    let sandbox = Sandbox::new();
    let repo = sandbox.create_git_repo(
        "upstreams/scratch_demo",
        &[("include/scratch_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "scratch_demo",
        &format!(
            "NAME scratch_demo\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nBUILD_ROOT SCRATCH\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET scratch_demo::scratch_demo INTERFACE include\n",
            portable_path(&repo)
        ),
    )
    .expect_err("BUILD_ROOT SCRATCH should be rejected off Linux");
    assert_error_contains(&error, "BUILD_ROOT SCRATCH is only supported on Linux");
}

#[test]
fn sync_rejects_build_root_oci_off_linux() {
    let sandbox = Sandbox::new();
    let repo = sandbox.create_git_repo(
        "upstreams/oci_demo",
        &[("include/oci_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "oci_demo",
        &format!(
            "NAME oci_demo\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nBUILD_ROOT OCI docker://docker.io/library/alpine:3.20\nTOOLCHAIN ROOTFS\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET oci_demo::oci_demo INTERFACE include\n",
            portable_path(&repo)
        ),
    )
    .expect_err("BUILD_ROOT OCI should be rejected off Linux");
    assert_error_contains(&error, "BUILD_ROOT OCI");
    assert_error_contains(&error, "only supported on Linux");
}

#[test]
fn sync_rejects_toolchain_rootfs_off_linux() {
    let sandbox = Sandbox::new();
    let repo = sandbox.create_git_repo(
        "upstreams/rootfs_demo",
        &[("include/rootfs_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "rootfs_demo",
        &format!(
            "NAME rootfs_demo\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nTOOLCHAIN ROOTFS\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET rootfs_demo::rootfs_demo INTERFACE include\n",
            portable_path(&repo)
        ),
    )
    .expect_err("TOOLCHAIN ROOTFS should be rejected off Linux");
    assert_error_contains(&error, "TOOLCHAIN ROOTFS is only supported on Linux");
}

#[test]
fn sync_rejects_non_host_native_build_request_off_linux() {
    let sandbox = Sandbox::new();
    let repo = sandbox.create_git_repo(
        "upstreams/cross_demo",
        &[("include/cross_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "cross_demo",
        &format!(
            "NAME cross_demo\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nBUILD_ARCH {}\nTARGET_ARCH {}\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET cross_demo::cross_demo INTERFACE include\n",
            portable_path(&repo),
            host_arch(),
            foreign_arch(),
        ),
    )
    .expect_err("non-host-native request should be rejected off Linux");
    assert_error_contains(
        &error,
        "non-Linux backends only support host-native BUILD_ROOT SYSTEM",
    );
}

fn sync_with_depofile(sandbox: &Sandbox, name: &str, depofile: &str) -> anyhow::Result<()> {
    sandbox.write(
        &format!("depofiles/local/{name}/{RELEASE_NAMESPACE}/1.0.0/main.DepoFile"),
        depofile,
    );
    sandbox.write(
        &format!("manifests/{name}.cmake"),
        &format!("depos_require({name} VERSION 1.0.0)\n"),
    );
    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join(format!("manifests/{name}.cmake")),
        executable: None,
    })?;
    Ok(())
}

fn static_library_file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    }
}

fn assert_error_contains(error: &anyhow::Error, expected: &str) {
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains(expected),
        "expected error containing {expected:?}, got: {rendered}"
    );
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn foreign_arch() -> &'static str {
    match host_arch().as_str() {
        "x86_64" => "aarch64",
        "aarch64" => "x86_64",
        "riscv64" => "x86_64",
        other => panic!("unsupported host arch {other}"),
    }
}

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(root.path().join("depofiles")).expect("create depofiles root");
        fs::create_dir_all(root.path().join(".run")).expect("create runtime root");
        Self { root }
    }

    fn depos_root(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    fn package_store_path(
        &self,
        name: &str,
        namespace: &str,
        version: &str,
        relative: &str,
    ) -> PathBuf {
        self.depos_root()
            .join("store")
            .join(depos::default_variant())
            .join(name)
            .join(namespace)
            .join(version)
            .join(relative)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
    }

    fn create_git_repo(&self, relative: &str, files: &[(&str, &str)]) -> PathBuf {
        let repo = self.root.path().join(relative);
        fs::create_dir_all(&repo).expect("create repo");
        for (path, contents) in files {
            let file_path = repo.join(path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).expect("create repo parent");
            }
            fs::write(file_path, contents).expect("write repo file");
        }
        run_command(&repo, ["git", "init", "--quiet"]);
        run_command(
            &repo,
            ["git", "config", "user.email", "codex@example.invalid"],
        );
        run_command(&repo, ["git", "config", "user.name", "Codex"]);
        run_command(&repo, ["git", "add", "."]);
        run_command(&repo, ["git", "commit", "--quiet", "-m", "init"]);
        repo
    }
}

fn run_command<const N: usize>(current_dir: &Path, argv: [&str; N]) {
    let status = Command::new(resolve_tool(argv[0]))
        .current_dir(current_dir)
        .args(&argv[1..])
        .status()
        .unwrap_or_else(|error| panic!("spawn {} failed: {error}", argv[0]));
    assert!(status.success(), "{} failed with {status}", argv[0]);
}

fn resolve_tool(tool: &str) -> PathBuf {
    if let Some(path) = std::env::var_os("PATH").and_then(|value| {
        let needs_windows_extension_search = cfg!(windows) && Path::new(tool).extension().is_none();
        let pathext = if needs_windows_extension_search {
            std::env::var_os("PATHEXT")
                .map(|value| {
                    value
                        .to_string_lossy()
                        .split(';')
                        .filter(|value| !value.is_empty())
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    vec![
                        ".COM".to_string(),
                        ".EXE".to_string(),
                        ".BAT".to_string(),
                        ".CMD".to_string(),
                    ]
                })
        } else {
            vec![String::new()]
        };
        std::env::split_paths(&value).find_map(|directory| {
            if needs_windows_extension_search {
                for extension in &pathext {
                    let candidate = directory.join(format!("{tool}{extension}"));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
            let direct = directory.join(tool);
            if direct.is_file() {
                return Some(direct);
            }
            None
        })
    }) {
        return path;
    }
    PathBuf::from(tool)
}
