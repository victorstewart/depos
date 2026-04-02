#![cfg(any(target_os = "macos", target_os = "windows"))]

// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use depos::{host_arch, sync_registry, SyncOptions};
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use tar::Builder;
use tempfile::TempDir;

const RELEASE_NAMESPACE: &str = "release";

#[test]
fn sync_builds_cargo_package_with_native_portable_backend() {
    let sandbox = Sandbox::new();
    let package_name = "portable_demo";
    let artifact_name = static_library_file_name(package_name);
    let artifact_stage_source = format!("cargo-target/release/{artifact_name}");
    let artifact_store_path = format!("lib/{artifact_name}");
    let archive = sandbox.create_source_archive(
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
            "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE URL {}\nBUILD_SYSTEM CARGO\nCARGO_BUILD cargo build --release --target-dir ${{DEPO_BUILD_DIR}}/cargo-target --manifest-path Cargo.toml\nSTAGE_FILE SOURCE include/{package_name}/demo.h include/{package_name}/demo.h\nSTAGE_FILE BUILD {artifact_stage_source} {artifact_store_path}\nTARGET {package_name}::{package_name} STATIC {artifact_store_path} INTERFACE include\n",
            portable_file_url(&archive)
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
    let archive = sandbox.create_source_archive(
        "upstreams/scratch_demo",
        &[("include/scratch_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "scratch_demo",
        &format!(
            "NAME scratch_demo\nVERSION 1.0.0\nSOURCE URL {}\nBUILD_ROOT SCRATCH\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET scratch_demo::scratch_demo INTERFACE include\n",
            portable_file_url(&archive)
        ),
    )
    .expect_err("BUILD_ROOT SCRATCH should be rejected off Linux");
    assert_error_contains(&error, "BUILD_ROOT SCRATCH is only supported on Linux");
}

#[test]
fn sync_rejects_build_root_oci_off_linux() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/oci_demo",
        &[("include/oci_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "oci_demo",
        &format!(
            "NAME oci_demo\nVERSION 1.0.0\nSOURCE URL {}\nBUILD_ROOT OCI docker://docker.io/library/alpine:3.20\nTOOLCHAIN ROOTFS\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET oci_demo::oci_demo INTERFACE include\n",
            portable_file_url(&archive)
        ),
    )
    .expect_err("BUILD_ROOT OCI should be rejected off Linux");
    assert_error_contains(&error, "BUILD_ROOT OCI");
    assert_error_contains(&error, "only supported on Linux");
}

#[test]
fn sync_rejects_toolchain_rootfs_off_linux() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/rootfs_demo",
        &[("include/rootfs_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "rootfs_demo",
        &format!(
            "NAME rootfs_demo\nVERSION 1.0.0\nSOURCE URL {}\nTOOLCHAIN ROOTFS\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET rootfs_demo::rootfs_demo INTERFACE include\n",
            portable_file_url(&archive)
        ),
    )
    .expect_err("TOOLCHAIN ROOTFS should be rejected off Linux");
    assert_error_contains(&error, "TOOLCHAIN ROOTFS is only supported on Linux");
}

#[test]
fn sync_rejects_non_host_native_build_request_off_linux() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/cross_demo",
        &[("include/cross_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "cross_demo",
        &format!(
            "NAME cross_demo\nVERSION 1.0.0\nSOURCE URL {}\nBUILD_ARCH {}\nTARGET_ARCH {}\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET cross_demo::cross_demo INTERFACE include\n",
            portable_file_url(&archive),
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

fn portable_file_url(path: &Path) -> String {
    let path = portable_path(path);
    if cfg!(windows) {
        format!("file:///{path}")
    } else {
        format!("file://{path}")
    }
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

    fn create_source_archive(&self, relative: &str, files: &[(&str, &str)]) -> PathBuf {
        let source_root = self.root.path().join(relative);
        fs::create_dir_all(&source_root).expect("create source root");
        for (path, contents) in files {
            let file_path = source_root.join(path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).expect("create source parent");
            }
            fs::write(file_path, contents).expect("write source file");
        }
        let archive_root = Path::new(relative)
            .file_name()
            .expect("archive root name")
            .to_string_lossy()
            .to_string();
        let archive_path = self.root.path().join(format!("{relative}.tar"));
        if let Some(parent) = archive_path.parent() {
            fs::create_dir_all(parent).expect("create archive parent");
        }
        let archive_file = File::create(&archive_path).expect("create archive");
        let mut builder = Builder::new(archive_file);
        for (path, _) in files {
            builder
                .append_path_with_name(source_root.join(path), format!("{archive_root}/{path}"))
                .expect("append archive entry");
        }
        builder.finish().expect("finish archive");
        archive_path
    }
}
