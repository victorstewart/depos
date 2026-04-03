#![cfg(any(target_os = "macos", target_os = "windows"))]

// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use depos::{host_arch, sync_registry, SyncOptions};
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
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
fn sync_builds_cmake_package_with_native_portable_backend() {
    let sandbox = Sandbox::new();
    let package_name = "portable_cmake_demo";
    let artifact_store_path = format!("lib/{}", static_library_file_name(package_name));
    let archive = sandbox.create_source_archive(
        "upstreams/portable_cmake_demo",
        &[
            (
                "CMakeLists.txt",
                &format!(
                    "cmake_minimum_required(VERSION 3.21)\nproject({package_name} LANGUAGES CXX)\nadd_library({package_name} STATIC src/demo.cpp)\ntarget_compile_features({package_name} PUBLIC cxx_std_20)\ntarget_include_directories({package_name} PUBLIC \"$<BUILD_INTERFACE:${{CMAKE_CURRENT_SOURCE_DIR}}/include>\" \"$<INSTALL_INTERFACE:include>\")\ninstall(TARGETS {package_name} ARCHIVE DESTINATION lib)\ninstall(DIRECTORY include/ DESTINATION include)\n"
                ),
            ),
            (
                "src/demo.cpp",
                "int portable_cmake_demo_add(int left, int right) {\n    return left + right;\n}\n",
            ),
            (
                "include/portable_cmake_demo/demo.h",
                "#pragma once\nint portable_cmake_demo_add(int left, int right);\n",
            ),
        ],
    );
    sandbox.write(
        &format!(
            "depofiles/local/{package_name}/{RELEASE_NAMESPACE}/1.0.0/main.DepoFile"
        ),
        &format!(
            "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE URL {}\nBUILD_SYSTEM CMAKE\nTARGET {package_name}::{package_name} STATIC {artifact_store_path} INTERFACE include\n",
            portable_file_url(&archive)
        ),
    );
    sandbox.write(
        "manifests/portable_cmake_demo.cmake",
        "depos_require(portable_cmake_demo VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/portable_cmake_demo.cmake"),
        executable: None,
    })
    .expect("sync should build portable native CMake package");

    assert_eq!(output.selected.len(), 1);
    assert!(sandbox
        .package_store_path(
            package_name,
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/portable_cmake_demo/demo.h"
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
fn sync_materializes_git_package_with_native_portable_backend() {
    let sandbox = Sandbox::new();
    let package_name = "portable_git_demo";
    let upstream = sandbox.create_git_repo(
        "upstreams/portable_git_demo",
        &[("include/portable_git_demo/demo.h", "#pragma once\n")],
    );
    sandbox.write(
        &format!(
            "depofiles/local/{package_name}/{RELEASE_NAMESPACE}/1.0.0/main.DepoFile"
        ),
        &format!(
            "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nTARGET {package_name}::{package_name} INTERFACE include\n",
            portable_path(&upstream)
        ),
    );
    sandbox.write(
        "manifests/portable_git_demo.cmake",
        "depos_require(portable_git_demo VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/portable_git_demo.cmake"),
        executable: None,
    })
    .expect("sync should materialize portable native git package");

    assert_eq!(output.selected.len(), 1);
    assert!(sandbox
        .package_store_path(
            package_name,
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/portable_git_demo/demo.h"
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
fn sync_rejects_build_root_oci_without_toolchain_rootfs_off_linux() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/oci_demo",
        &[("include/oci_demo/demo.h", "// demo\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "oci_demo",
        &format!(
            "NAME oci_demo\nVERSION 1.0.0\nSOURCE URL {}\nBUILD_ROOT OCI docker://docker.io/library/alpine:3.20\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD cargo --version\nTARGET oci_demo::oci_demo INTERFACE include\n",
            portable_file_url(&archive)
        ),
    )
    .expect_err("BUILD_ROOT OCI without TOOLCHAIN ROOTFS should be rejected off Linux");
    assert_error_contains(&error, "BUILD_ROOT OCI");
    assert_error_contains(&error, "without TOOLCHAIN ROOTFS");
}

#[test]
fn sync_rejects_toolchain_rootfs_without_oci_off_linux() {
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
    .expect_err("TOOLCHAIN ROOTFS without BUILD_ROOT OCI should be rejected off Linux");
    assert_error_contains(&error, "TOOLCHAIN ROOTFS without BUILD_ROOT OCI");
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
    assert_error_contains(&error, "without BUILD_ROOT OCI");
}

#[test]
fn sync_builds_linux_oci_package_with_provider_when_enabled() {
    if !linux_provider_tests_enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let package_name = "linux_provider_demo";
    let archive = sandbox.create_source_archive(
        "upstreams/linux_provider_demo",
        &[("payload/demo.h", "#pragma once\n")],
    );
    sync_with_depofile(
        &sandbox,
        package_name,
        &format!(
            "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE URL {}\nBUILD_ROOT OCI docker://docker.io/library/alpine:3.20\nTOOLCHAIN ROOTFS\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/{package_name}/demo.h\"\nEOF\nTARGET {package_name}::{package_name} INTERFACE include\n",
            portable_file_url(&archive)
        ),
    )
    .expect("BUILD_ROOT OCI should route through the Linux provider");

    assert!(sandbox
        .package_store_path_for_target_arch(
            package_name,
            RELEASE_NAMESPACE,
            "1.0.0",
            host_arch().as_str(),
            &format!("include/{package_name}/demo.h"),
        )
        .is_file());
}

#[test]
fn sync_builds_linux_oci_cargo_binary_with_provider_when_enabled() {
    if !linux_provider_tests_enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let package_name = "linux_provider_cargo_demo";
    let archive = sandbox.create_source_archive(
        "upstreams/linux_provider_cargo_demo",
        &[
            (
                "Cargo.toml",
                &format!(
                    "[package]\nname = \"{package_name}\"\nversion = \"1.0.0\"\nedition = \"2021\"\n"
                ),
            ),
            (
                "src/main.rs",
                "fn main() {\n    println!(\"linux-provider-cargo-demo\");\n}\n",
            ),
        ],
    );
    sync_with_depofile(
        &sandbox,
        package_name,
        &format!(
            "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE URL {}\nBUILD_ROOT OCI docker://docker.io/library/ubuntu:24.04\nTOOLCHAIN ROOTFS\nBUILD_SYSTEM CARGO\nSTAGE_FILE BUILD cargo-target/release/{package_name} bin/{package_name}\nARTIFACT bin/{package_name}\n",
            portable_file_url(&archive)
        ),
    )
    .expect("provider-backed OCI build should produce a Linux cargo binary");

    assert!(sandbox
        .package_store_path_for_target_arch(
            package_name,
            RELEASE_NAMESPACE,
            "1.0.0",
            host_arch().as_str(),
            &format!("bin/{package_name}"),
        )
        .is_file());
}

#[cfg(target_os = "windows")]
#[test]
fn sync_reuses_wsl_provider_bootstrap_state_across_oci_builds() {
    if !linux_provider_tests_enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let provider_root = unique_provider_root("reuse");

    let first_archive = sandbox.create_source_archive(
        "upstreams/provider_reuse_first",
        &[("payload/demo.h", "#pragma once\n")],
    );
    with_env_vars(
        &[
            ("DEPOS_LINUX_PROVIDER", Some("wsl2")),
            ("DEPOS_LINUX_PROVIDER_ROOT", Some(&provider_root)),
        ],
        || {
            sync_with_depofile(
                &sandbox,
                "provider_reuse_first",
                &provider_header_depofile(
                    "provider_reuse_first",
                    &portable_file_url(&first_archive),
                ),
            )
        },
    )
    .expect("first OCI build should cold-bootstrap the provider");

    let second_archive = sandbox.create_source_archive(
        "upstreams/provider_reuse_second",
        &[("payload/demo.h", "#pragma once\n")],
    );
    with_env_vars(
        &[
            ("DEPOS_LINUX_PROVIDER", Some("wsl2")),
            ("DEPOS_LINUX_PROVIDER_ROOT", Some(&provider_root)),
        ],
        || {
            sync_with_depofile(
                &sandbox,
                "provider_reuse_second",
                &provider_header_depofile(
                    "provider_reuse_second",
                    &portable_file_url(&second_archive),
                ),
            )
        },
    )
    .expect("second OCI build should reuse the provider bootstrap");

    let second_log =
        sandbox.read_materialization_log("provider_reuse_second", RELEASE_NAMESPACE, "1.0.0");
    assert!(
        second_log.contains("provider bootstrap: warm"),
        "expected second build log to show warm provider bootstrap, got:\n{second_log}"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn sync_records_wsl_provider_metadata_under_runtime_root() {
    if !linux_provider_tests_enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let provider_root = unique_provider_root("metadata");
    let distro = wsl_distro_for_test();
    let archive = sandbox.create_source_archive(
        "upstreams/provider_metadata_demo",
        &[("payload/demo.h", "#pragma once\n")],
    );

    with_env_vars(
        &[
            ("DEPOS_LINUX_PROVIDER", Some("wsl2")),
            ("DEPOS_LINUX_PROVIDER_ROOT", Some(&provider_root)),
            ("DEPOS_WSL_DISTRO", Some(&distro)),
        ],
        || {
            sync_with_depofile(
                &sandbox,
                "provider_metadata_demo",
                &provider_header_depofile("provider_metadata_demo", &portable_file_url(&archive)),
            )
        },
    )
    .expect("provider-backed OCI build should record provider metadata");

    let metadata = read_wsl_text_file(&distro, &format!("{provider_root}/provider-metadata.env"));
    assert!(
        metadata.contains("provider_kind=wsl2"),
        "expected WSL provider metadata, got:\n{metadata}"
    );
    assert!(
        metadata.contains(&format!("provider_identity={distro}")),
        "expected provider identity in metadata, got:\n{metadata}"
    );
    assert!(
        metadata.contains(&format!("runtime_root={provider_root}")),
        "expected runtime root in metadata, got:\n{metadata}"
    );
    assert!(
        metadata.contains("runtime_layout_version=v1"),
        "expected runtime layout version in metadata, got:\n{metadata}"
    );
    assert!(
        metadata.contains("bootstrap_version=v1"),
        "expected bootstrap version in metadata, got:\n{metadata}"
    );
    assert!(
        metadata.contains(&format!(
            "bootstrap_stamp={provider_root}/bootstrap-v1.stamp"
        )),
        "expected bootstrap stamp in metadata, got:\n{metadata}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sync_reports_missing_direct_apple_virtualization_helper_for_oci_requests() {
    if linux_provider_tests_enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/avf_missing_demo",
        &[("payload/demo.h", "#pragma once\n")],
    );
    let error = sync_with_depofile(
        &sandbox,
        "avf_missing_demo",
        &format!(
            "NAME avf_missing_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE URL {}\nBUILD_ROOT OCI docker://docker.io/library/alpine:3.20\nTOOLCHAIN ROOTFS\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/avf_missing_demo/demo.h\"\nEOF\nTARGET avf_missing_demo::avf_missing_demo INTERFACE include\n",
            portable_file_url(&archive)
        ),
    )
    .expect_err("macOS OCI provider should require a direct Apple Virtualization helper");
    assert_error_contains(&error, "direct Apple Virtualization helper");
    assert_error_contains(&error, "DEPOS_APPLE_VIRTUALIZATION_HELPER");
}

#[test]
fn sync_reports_invalid_linux_provider_selection_for_oci_requests() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/invalid_provider_selection_demo",
        &[("payload/demo.h", "#pragma once\n")],
    );
    let error = with_env_vars(&[("DEPOS_LINUX_PROVIDER", Some("bogus"))], || {
        sync_with_depofile(
            &sandbox,
            "invalid_provider_selection_demo",
            &provider_header_depofile(
                "invalid_provider_selection_demo",
                &portable_file_url(&archive),
            ),
        )
    })
    .expect_err("invalid provider selection should be rejected");
    assert_error_contains(&error, "unsupported DEPOS_LINUX_PROVIDER");
    assert_error_contains(&error, "auto, wsl2, mac-local");
}

#[test]
fn sync_reports_invalid_linux_provider_root_for_oci_requests() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/invalid_provider_root_demo",
        &[("payload/demo.h", "#pragma once\n")],
    );
    let error = with_env_vars(
        &[("DEPOS_LINUX_PROVIDER_ROOT", Some("relative-root"))],
        || {
            sync_with_depofile(
                &sandbox,
                "invalid_provider_root_demo",
                &provider_header_depofile(
                    "invalid_provider_root_demo",
                    &portable_file_url(&archive),
                ),
            )
        },
    )
    .expect_err("invalid provider root should be rejected");
    assert_error_contains(
        &error,
        "DEPOS_LINUX_PROVIDER_ROOT must be an absolute Linux path",
    );
    assert_error_contains(&error, "relative-root");
}

#[cfg(target_os = "windows")]
#[test]
fn sync_rejects_mac_local_provider_selection_on_windows() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/windows_wrong_provider_demo",
        &[("payload/demo.h", "#pragma once\n")],
    );
    let error = with_env_vars(&[("DEPOS_LINUX_PROVIDER", Some("mac-local"))], || {
        sync_with_depofile(
            &sandbox,
            "windows_wrong_provider_demo",
            &provider_header_depofile("windows_wrong_provider_demo", &portable_file_url(&archive)),
        )
    })
    .expect_err("mac-local provider selection should be rejected on Windows");
    assert_error_contains(
        &error,
        "DEPOS_LINUX_PROVIDER=mac-local is not supported on Windows",
    );
    assert_error_contains(&error, "use auto or wsl2");
}

#[cfg(target_os = "windows")]
#[test]
fn sync_reports_missing_explicit_wsl_distro_for_oci_requests() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/windows_missing_distro_demo",
        &[("payload/demo.h", "#pragma once\n")],
    );
    let error = with_env_vars(
        &[
            ("DEPOS_LINUX_PROVIDER", Some("wsl2")),
            ("DEPOS_WSL_DISTRO", Some("depos-does-not-exist")),
        ],
        || {
            sync_with_depofile(
                &sandbox,
                "windows_missing_distro_demo",
                &provider_header_depofile(
                    "windows_missing_distro_demo",
                    &portable_file_url(&archive),
                ),
            )
        },
    )
    .expect_err("missing explicit WSL distro should be reported clearly");
    assert_error_contains(&error, "depos-does-not-exist");
    assert_error_contains(&error, "install/configure WSL");
}

#[cfg(target_os = "macos")]
#[test]
fn sync_rejects_wsl2_provider_selection_on_macos() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_source_archive(
        "upstreams/macos_wrong_provider_demo",
        &[("payload/demo.h", "#pragma once\n")],
    );
    let error = with_env_vars(&[("DEPOS_LINUX_PROVIDER", Some("wsl2"))], || {
        sync_with_depofile(
            &sandbox,
            "macos_wrong_provider_demo",
            &provider_header_depofile("macos_wrong_provider_demo", &portable_file_url(&archive)),
        )
    })
    .expect_err("wsl2 provider selection should be rejected on macOS");
    assert_error_contains(
        &error,
        "DEPOS_LINUX_PROVIDER=wsl2 is not supported on macOS",
    );
    assert_error_contains(&error, "use auto or mac-local");
}

#[test]
fn sync_builds_cross_target_linux_oci_package_with_provider_when_enabled() {
    if !linux_provider_tests_enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let package_name = "linux_provider_cross_demo";
    let archive = sandbox.create_source_archive(
        "upstreams/linux_provider_cross_demo",
        &[(
            &format!("payload/{}-to-{}.h", host_arch(), foreign_arch()),
            "// cross target\n",
        )],
    );
    sync_with_depofile(
        &sandbox,
        package_name,
        &format!(
            "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE URL {}\nBUILD_ROOT OCI docker://docker.io/library/alpine:3.20\nTOOLCHAIN ROOTFS\nBUILD_ARCH {}\nTARGET_ARCH {}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/${{DEPO_BUILD_ARCH}}-to-${{DEPO_TARGET_ARCH}}.h\" \"${{DEPO_PREFIX}}/include/{package_name}/demo.h\"\nEOF\nTARGET {package_name}::{package_name} INTERFACE include\n",
            portable_file_url(&archive),
            host_arch(),
            foreign_arch(),
        ),
    )
    .expect("cross-target BUILD_ROOT OCI should route through the Linux provider");

    assert!(sandbox
        .package_store_path_for_target_arch(
            package_name,
            RELEASE_NAMESPACE,
            "1.0.0",
            foreign_arch(),
            &format!("include/{package_name}/demo.h"),
        )
        .is_file());
}

#[test]
fn sync_builds_cross_target_linux_oci_cargo_binary_with_provider_when_enabled() {
    if !linux_provider_tests_enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let package_name = "linux_provider_cross_cargo_demo";
    let target_arch = foreign_arch();
    let target_triple = linux_target_triple(target_arch);
    let archive = sandbox.create_source_archive(
        "upstreams/linux_provider_cross_cargo_demo",
        &[
            (
                "Cargo.toml",
                &format!(
                    "[package]\nname = \"{package_name}\"\nversion = \"1.0.0\"\nedition = \"2021\"\n"
                ),
            ),
            (
                "src/main.rs",
                "fn main() {\n    println!(\"linux-provider-cross-cargo-demo\");\n}\n",
            ),
        ],
    );
    sync_with_depofile(
        &sandbox,
        package_name,
        &format!(
            "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE URL {}\nBUILD_ROOT OCI docker://docker.io/library/ubuntu:24.04\nTOOLCHAIN ROOTFS\nBUILD_ARCH {}\nTARGET_ARCH {}\nBUILD_SYSTEM CARGO\nSTAGE_FILE BUILD cargo-target/{target_triple}/release/{package_name} bin/{package_name}\nARTIFACT bin/{package_name}\n",
            portable_file_url(&archive),
            host_arch(),
            target_arch,
        ),
    )
    .expect("provider-backed OCI build should cross-compile a Linux cargo binary");

    assert!(sandbox
        .package_store_path_for_target_arch(
            package_name,
            RELEASE_NAMESPACE,
            "1.0.0",
            target_arch,
            &format!("bin/{package_name}"),
        )
        .is_file());
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

fn linux_target_triple(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        "riscv64" => "riscv64gc-unknown-linux-gnu",
        other => panic!("unsupported linux target triple arch {other}"),
    }
}

fn linux_provider_tests_enabled() -> bool {
    matches!(
        std::env::var("DEPOS_TEST_LINUX_PROVIDER").as_deref(),
        Ok("1")
    )
}

#[cfg(target_os = "windows")]
fn unique_provider_root(label: &str) -> String {
    format!(
        "/tmp/depos-provider-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    )
}

#[cfg(target_os = "windows")]
fn read_wsl_text_file(distro: &str, path: &str) -> String {
    let output = Command::new("wsl.exe")
        .args([
            "-d",
            distro,
            "--",
            "bash",
            "-lc",
            &format!("cat {}", bash_quote(path)),
        ])
        .output()
        .expect("spawn wsl.exe");
    assert!(
        output.status.success(),
        "wsl.exe failed reading {path}: stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

#[cfg(target_os = "windows")]
fn wsl_distro_for_test() -> String {
    if let Ok(explicit) = std::env::var("DEPOS_WSL_DISTRO") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let output = Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .output()
        .expect("query WSL distributions");
    assert!(
        output.status.success(),
        "wsl.exe --list --quiet failed: stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .expect("expected at least one installed WSL distribution")
}

#[cfg(target_os = "windows")]
fn bash_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

fn provider_header_depofile(package_name: &str, source_url: &str) -> String {
    format!(
        "NAME {package_name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE URL {source_url}\nBUILD_ROOT OCI docker://docker.io/library/alpine:3.20\nTOOLCHAIN ROOTFS\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/{package_name}/demo.h\"\nEOF\nTARGET {package_name}::{package_name} INTERFACE include\n"
    )
}

struct Sandbox {
    root: TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl Sandbox {
    fn new() -> Self {
        let guard = portable_test_lock()
            .lock()
            .expect("portable integration test lock");
        let root = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(root.path().join("depofiles")).expect("create depofiles root");
        fs::create_dir_all(root.path().join(".run")).expect("create runtime root");
        Self {
            root,
            _guard: guard,
        }
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
        self.package_store_path_for_target_arch(
            name,
            namespace,
            version,
            host_arch().as_str(),
            relative,
        )
    }

    fn package_store_path_for_target_arch(
        &self,
        name: &str,
        namespace: &str,
        version: &str,
        target_arch: &str,
        relative: &str,
    ) -> PathBuf {
        self.depos_root()
            .join("store")
            .join(format!("{target_arch}-{target_arch}_v1"))
            .join(name)
            .join(namespace)
            .join(version)
            .join(relative)
    }

    #[cfg(target_os = "windows")]
    fn read_materialization_log(&self, name: &str, namespace: &str, version: &str) -> String {
        fs::read_to_string(
            self.depos_root()
                .join(".run")
                .join("logs")
                .join(name)
                .join(namespace)
                .join(format!("{version}.log")),
        )
        .expect("read materialization log")
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
        run_test_command(&repo, "git", ["init", "--quiet"]);
        run_test_command(
            &repo,
            "git",
            ["config", "user.email", "codex@example.invalid"],
        );
        run_test_command(&repo, "git", ["config", "user.name", "Codex"]);
        run_test_command(&repo, "git", ["add", "."]);
        run_test_command(&repo, "git", ["commit", "--quiet", "-m", "init"]);
        repo
    }
}

fn run_test_command<const N: usize>(current_dir: &Path, executable: &str, args: [&str; N]) {
    let status = Command::new(executable)
        .args(args)
        .current_dir(current_dir)
        .status()
        .expect("spawn command");
    assert!(
        status.success(),
        "command failed: {} {:?}",
        executable,
        args
    );
}

fn portable_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn with_env_vars<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
    let _guard = TestEnvGuard::new(vars);
    f()
}

struct TestEnvGuard {
    saved: Vec<(String, Option<OsString>)>,
}

impl TestEnvGuard {
    fn new(vars: &[(&str, Option<&str>)]) -> Self {
        let saved = vars
            .iter()
            .map(|(name, _)| ((*name).to_string(), std::env::var_os(name)))
            .collect::<Vec<_>>();
        for (name, value) in vars {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        Self { saved }
    }
}

impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
