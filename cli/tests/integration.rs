// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use depos::{
    collect_statuses, default_variant, host_arch, parse_depofile, parse_manifest,
    register_depofile, registry_dir_from_manifest, sync_registry, unregister_depofile,
    PackageState, RegisterOptions, RequestMode, RequestSource, StageKind, StatusOptions,
    SyncOptions, UnregisterOptions,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use tempfile::TempDir;

const RELEASE_NAMESPACE: &str = "release";

#[test]
fn sync_generates_registry_for_materialized_packages() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "manifests/example.cmake",
        "depos_require(bitsery)\ndepos_require(itoa)\ndepos_require(zlib VERSION 1.3.2)\n",
    );
    sandbox.write_store("include/bitsery/bitsery.h", "// bitsery\n");
    sandbox.write_store("include/itoa/jeaiii_to_text.h", "// itoa\n");
    sandbox.write_store("include/zlib.h", "// zlib\n");
    sandbox.write_store("include/zconf.h", "// zconf\n");
    sandbox.write_store("lib/libz.a", "archive\n");

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/example.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should succeed");

    let targets = fs::read_to_string(output.targets_file).expect("targets.cmake should exist");
    assert!(targets.contains("add_library(bitsery::bitsery ALIAS _depos_bitsery_release_5_2_3_t0)"));
    assert!(targets.contains("add_library(itoa::itoa ALIAS _depos_itoa_release_main_t0)"));
    assert!(targets.contains("add_library(zlib::zlib ALIAS _depos_zlib_release_1_3_2_t0)"));
    assert!(targets.contains("/zlib/release/1.3.2/lib/libz.a"));

    let lock = fs::read_to_string(output.lock_file).expect("lock.cmake should exist");
    assert!(lock.contains("bitsery[release]@5.2.3|AUTO|LATEST|_|_"));
    assert!(lock.contains("itoa[release]@main|AUTO|LATEST|_|_"));
    assert!(lock.contains("zlib[release]@1.3.2|AUTO|EXACT|1.3.2|_"));
}

#[test]
fn sync_emits_interface_metadata_and_honors_primary_target() {
    let sandbox = Sandbox::new();
    let provider_repo = sandbox.create_git_repo(
        "repos/provider",
        &[
            ("include/provider/provider.h", "// provider\n"),
            ("lib/libprovider.a", "archive\n"),
        ],
    );
    let consumer_repo = sandbox.create_git_repo(
        "repos/consumer",
        &[("include/consumer/consumer.h", "// consumer\n")],
    );
    sandbox.write(
        "tmp/provider.DepoFile",
        &format!(
            "NAME provider\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nPRIMARY_TARGET provider::runtime\nTARGET provider::headers INTERFACE include\nDEFINES provider::headers PROVIDER_HEADER_ONLY=1 PAGE_SIZE=4096\nOPTIONS provider::headers -Winvalid-pch\nFEATURES provider::headers cxx_std_20\nTARGET provider::runtime STATIC lib/libprovider.a\n",
            provider_repo.display()
        ),
    );
    sandbox.write(
        "tmp/consumer.DepoFile",
        &format!(
            "NAME consumer\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nDEPENDS provider VERSION 1.0.0\nTARGET consumer::consumer INTERFACE include\n",
            consumer_repo.display()
        ),
    );
    sandbox.write(
        "manifests/consumer.cmake",
        "depos_require(consumer VERSION 1.0.0)\n",
    );

    register_depofile(&RegisterOptions {
        depos_root: sandbox.depos_root(),
        file: sandbox.depos_root().join("tmp/provider.DepoFile"),
        namespace: RELEASE_NAMESPACE.to_string(),
    })
    .expect("register provider");
    register_depofile(&RegisterOptions {
        depos_root: sandbox.depos_root(),
        file: sandbox.depos_root().join("tmp/consumer.DepoFile"),
        namespace: RELEASE_NAMESPACE.to_string(),
    })
    .expect("register consumer");

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/consumer.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should succeed");

    let targets = fs::read_to_string(output.targets_file).expect("targets.cmake should exist");
    assert!(
        targets.contains("INTERFACE_COMPILE_DEFINITIONS \"PROVIDER_HEADER_ONLY=1;PAGE_SIZE=4096\"")
    );
    assert!(targets.contains("INTERFACE_COMPILE_OPTIONS \"-Winvalid-pch\""));
    assert!(targets.contains("INTERFACE_COMPILE_FEATURES \"cxx_std_20\""));
    assert!(targets.contains("INTERFACE_LINK_LIBRARIES \"_depos_provider_release_1_0_0_t1\""));
    assert!(!targets.contains("INTERFACE_LINK_LIBRARIES \"_depos_provider_release_1_0_0_t0\"\n"));
}

#[test]
fn register_refreshes_green_status_and_unregister_cleans_up() {
    let sandbox = Sandbox::new();
    let variant = default_variant();
    sandbox.write_package_store(
        "demo",
        RELEASE_NAMESPACE,
        "1.0.0",
        "include/demo/demo.h",
        "// demo\n",
    );
    sandbox.write("tmp/demo.DepoFile", depofile_demo().as_str());

    let status = register_depofile(&RegisterOptions {
        depos_root: sandbox.depos_root(),
        file: sandbox.depos_root().join("tmp/demo.DepoFile"),
        namespace: RELEASE_NAMESPACE.to_string(),
    })
    .expect("register should succeed");
    assert_eq!(status.state, PackageState::Green);

    let statuses = collect_statuses(&StatusOptions {
        depos_root: sandbox.depos_root(),
        name: Some("demo".to_string()),
        namespace: None,
        version: Some("1.0.0".to_string()),
        refresh: false,
    })
    .expect("status should succeed");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, PackageState::Green);
    assert!(statuses[0].message.contains(&format!("store/{variant}")));

    unregister_depofile(&UnregisterOptions {
        depos_root: sandbox.depos_root(),
        name: "demo".to_string(),
        namespace: RELEASE_NAMESPACE.to_string(),
        version: "1.0.0".to_string(),
    })
    .expect("unregister should succeed");
    assert!(!sandbox
        .depos_root()
        .join("depofiles/local/demo/release/1.0.0/main.DepoFile")
        .exists());
}

#[test]
fn register_assigns_namespace_from_registry_options() {
    let sandbox = Sandbox::new();
    sandbox.write_package_store(
        "demo",
        "dev-main",
        "1.0.0",
        "include/demo/demo.h",
        "// demo\n",
    );
    sandbox.write("tmp/demo.DepoFile", depofile_demo().as_str());

    let status = register_depofile(&RegisterOptions {
        depos_root: sandbox.depos_root(),
        file: sandbox.depos_root().join("tmp/demo.DepoFile"),
        namespace: "dev-main".to_string(),
    })
    .expect("register with explicit namespace should succeed");

    assert_eq!(status.namespace, "dev-main");
    assert_eq!(status.state, PackageState::Green);
    assert!(sandbox
        .depos_root()
        .join("depofiles/local/demo/dev-main/1.0.0/main.DepoFile")
        .exists());
}

#[test]
fn register_quarantines_missing_dependency() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/app.DepoFile",
        "NAME app\nVERSION 1.0.0\nTARGET app::app INTERFACE include\nDEPENDS missing VERSION 1.2.3\n",
    );

    let status = register_depofile(&RegisterOptions {
        depos_root: sandbox.depos_root(),
        file: sandbox.depos_root().join("tmp/app.DepoFile"),
        namespace: RELEASE_NAMESPACE.to_string(),
    })
    .expect("register should succeed");
    assert_eq!(status.state, PackageState::Quarantined);
    assert!(status.message.contains("missing"));
}

#[test]
fn manifest_and_depofile_parsers_cover_known_surface() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/manifest.cmake",
        "depos_require(zlib VERSION 1.3.2 SOURCE DEPO)\ndepos_require(bitsery MIN_VERSION 5.0.0 NAMESPACE dev-main AS bitsery_dev)\n",
    );
    let requests = parse_manifest(&sandbox.depos_root().join("tmp/manifest.cmake"))
        .expect("manifest parsing should succeed");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].source, RequestSource::Depo);
    assert_eq!(requests[1].mode, RequestMode::Minimum("5.0.0".to_string()));
    assert_eq!(requests[1].namespace, "dev-main");
    assert_eq!(requests[1].alias.as_deref(), Some("bitsery_dev"));

    sandbox.write("tmp/spec.DepoFile", depofile_demo().as_str());
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/spec.DepoFile"))
        .expect("depofile parsing should succeed");
    assert_eq!(spec.name, "demo");
    assert_eq!(spec.namespace, RELEASE_NAMESPACE);
    assert_eq!(spec.targets[0].name, "demo::demo");
    assert!(spec.targets[0].link_libraries.is_empty());

    sandbox.write(
        "tmp/arch.DepoFile",
        &format!(
            "NAME arch_demo\nVERSION 1.0.0\nBUILD_ARCH {}\nTARGET_ARCH amd64\nARTIFACT include/demo.h\n",
            foreign_arch()
        ),
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/arch.DepoFile"))
        .expect("depofile arch parsing should succeed");
    assert_eq!(spec.build_arch, foreign_arch());
    assert_eq!(spec.target_arch, "x86_64");

    sandbox.write(
        "tmp/v2.DepoFile",
        "NAME v2_demo\nVERSION 1.0.0\nSOURCE GIT /tmp/example HEAD\nSOURCE_SUBDIR subdir\nTARGET v2_demo::v2_demo STATIC lib/libv2_demo.a INTERFACE include\nLINK v2_demo::v2_demo pthread\nBUILD_SYSTEM CMAKE\nCMAKE_DEFINE BUILD_SHARED_LIBS=OFF\n",
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/v2.DepoFile"))
        .expect("v2 depofile parsing should succeed");
    assert_eq!(spec.source_subdir, Some(PathBuf::from("subdir")));
    assert_eq!(spec.targets[0].name, "v2_demo::v2_demo");
    assert_eq!(
        spec.targets[0].static_path,
        Some(PathBuf::from("lib/libv2_demo.a"))
    );
    assert_eq!(spec.targets[0].include_dirs, vec![PathBuf::from("include")]);
    assert_eq!(spec.targets[0].link_libraries, vec!["pthread".to_string()]);
    assert_eq!(spec.configure[0][0], "cmake");
    assert!(spec.configure[0]
        .iter()
        .any(|arg| arg == "-DBUILD_SHARED_LIBS=OFF"));

    sandbox.write(
        "tmp/v2_direct.DepoFile",
        "NAME v2_direct_demo\nVERSION 1.0.0\nSOURCE GIT /tmp/example HEAD\nTARGET v2_direct_demo::v2_direct_demo STATIC lib/libv2_direct_demo.a INTERFACE include\nBUILD_SYSTEM CMAKE\nCMAKE_DEFINE BUILD_SHARED_LIBS=OFF\nCMAKE_BUILD cmake --build . --parallel\nCMAKE_INSTALL cmake --install .\n",
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/v2_direct.DepoFile"))
        .expect("direct-command depofile parsing should succeed");
    assert_eq!(spec.build[0][0], "cmake");
    assert_eq!(spec.build[0][1], "--build");
    assert_eq!(spec.build[0][2], ".");
    assert_eq!(spec.install[0], vec!["cmake", "--install", "."]);
    assert!(spec.configure[0]
        .iter()
        .any(|arg| arg == "-DBUILD_SHARED_LIBS=OFF"));

    sandbox.write(
        "tmp/v2_multi_role.DepoFile",
        "NAME v2_multi_role_demo\nVERSION 1.0.0\nTARGET v2_multi_role_demo::v2_multi_role_demo STATIC lib/libv2_multi_role_demo.a\nTARGET v2_multi_role_demo::v2_multi_role_demo SHARED lib/libv2_multi_role_demo.so\nTARGET v2_multi_role_demo::v2_multi_role_demo OBJECT lib/v2_multi_role_demo.o\nTARGET v2_multi_role_demo::v2_multi_role_demo INTERFACE include generated/include\n",
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/v2_multi_role.DepoFile"))
        .expect("multi-role target parsing should succeed");
    assert_eq!(
        spec.targets[0].static_path,
        Some(PathBuf::from("lib/libv2_multi_role_demo.a"))
    );
    assert_eq!(
        spec.targets[0].shared_path,
        Some(PathBuf::from("lib/libv2_multi_role_demo.so"))
    );
    assert_eq!(
        spec.targets[0].object_path,
        Some(PathBuf::from("lib/v2_multi_role_demo.o"))
    );
    assert_eq!(
        spec.targets[0].include_dirs,
        vec![PathBuf::from("include"), PathBuf::from("generated/include")]
    );

    sandbox.write(
        "tmp/manual_install.DepoFile",
        "NAME manual_install_demo\nVERSION 1.0.0\nSOURCE GIT /tmp/example HEAD\nBUILD_SYSTEM MANUAL\nSTAGE_TREE SOURCE include/manual_install_demo\nSTAGE_FILE SOURCE metadata/license.txt share/licenses/manual_install_demo/LICENSE\nTARGET manual_install_demo::manual_install_demo INTERFACE include\nARTIFACT share/licenses/manual_install_demo/LICENSE\n",
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/manual_install.DepoFile"))
        .expect("manual install depofile parsing should succeed");
    assert_eq!(spec.stage_entries.len(), 2);
    assert_eq!(spec.stage_entries[0].kind, StageKind::Tree);
    assert_eq!(
        spec.stage_entries[0].source,
        PathBuf::from("include/manual_install_demo")
    );
    assert_eq!(spec.stage_entries[1].kind, StageKind::File);
    assert_eq!(
        spec.stage_entries[1].destination,
        PathBuf::from("share/licenses/manual_install_demo/LICENSE")
    );

    sandbox.write(
        "tmp/manual_direct.DepoFile",
        "NAME manual_direct_demo\nVERSION 1.0.0\nSOURCE GIT /tmp/example HEAD\nBUILD_SYSTEM MANUAL\nTARGET manual_direct_demo::manual_direct_demo INTERFACE include\n",
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/manual_direct.DepoFile"))
        .expect("direct manual depofile parsing should succeed");
    assert!(spec.configure.is_empty());
    assert!(spec.build.is_empty());
    assert!(spec.install.is_empty());
    assert!(spec.stage_entries.is_empty());

    sandbox.write(
        "tmp/env_and_submodules.DepoFile",
        "NAME env_and_submodules_demo\nVERSION 1.0.0\nSOURCE GIT /tmp/example HEAD\nGIT_SUBMODULES RECURSIVE\nBUILD_SYSTEM AUTOCONF\nAUTOCONF_CONFIGURE_SH <<'EOF'\n./configure --prefix=\"${DEPO_PREFIX}\" --libdir=\"${DEPO_PREFIX}/lib\" --disable-shared\nEOF\nTARGET env_and_submodules_demo::env_and_submodules_demo STATIC lib/libenv_and_submodules_demo.a INTERFACE include\n",
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/env_and_submodules.DepoFile"))
        .expect("explicit configure command and git submodules should parse");
    assert!(spec.git_submodules_recursive);
    assert_eq!(
        spec.configure,
        vec![vec![
            "sh".to_string(),
            "-eu".to_string(),
            "-c".to_string(),
            "./configure --prefix=\"${DEPO_PREFIX}\" --libdir=\"${DEPO_PREFIX}/lib\" --disable-shared\n"
                .to_string(),
        ]]
    );

    let error = {
        sandbox.write(
            "tmp/legacy_target_surface.DepoFile",
            "NAME legacy_target_surface\nVERSION 1.0.0\nTARGET legacy_target_surface::legacy_target_surface STATIC lib/liblegacy_target_surface.a include\n",
        );
        parse_depofile(&sandbox.depos_root().join("tmp/legacy_target_surface.DepoFile"))
    }
    .expect_err("legacy compact target include syntax should fail");
    assert!(format!("{error:#}").contains("TARGET requires"));

    sandbox.write(
        "tmp/namespace_in_file.DepoFile",
        "NAME bad\nNAMESPACE dev-main\nVERSION 1.0.0\nARTIFACT include/bad.h\n",
    );
    let error = parse_depofile(&sandbox.depos_root().join("tmp/namespace_in_file.DepoFile"))
        .expect_err("top-level NAMESPACE should be rejected");
    assert!(format!("{error:#}").contains("depos register --namespace"));
}

#[test]
fn depofile_merges_target_facets_across_lines_and_rejects_duplicate_artifacts() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/target_facets.DepoFile",
        "NAME target_facets\nVERSION 1.0.0\nTARGET target_facets::core STATIC lib/libtarget_facets.a\nTARGET target_facets::core SHARED lib/libtarget_facets.so\nTARGET target_facets::core OBJECT lib/target_facets.o\nTARGET target_facets::core INTERFACE include generated/include\n",
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/target_facets.DepoFile"))
        .expect("multiline target facets should merge");
    assert_eq!(spec.targets.len(), 1);
    assert_eq!(
        spec.targets[0].static_path,
        Some(PathBuf::from("lib/libtarget_facets.a"))
    );
    assert_eq!(
        spec.targets[0].shared_path,
        Some(PathBuf::from("lib/libtarget_facets.so"))
    );
    assert_eq!(
        spec.targets[0].object_path,
        Some(PathBuf::from("lib/target_facets.o"))
    );
    assert_eq!(
        spec.targets[0].include_dirs,
        vec![PathBuf::from("include"), PathBuf::from("generated/include")]
    );

    sandbox.write(
        "tmp/target_facets_single_line.DepoFile",
        "NAME target_facets_single_line\nVERSION 1.0.0\nTARGET target_facets_single_line::core STATIC lib/libtarget_facets_single_line.a SHARED lib/libtarget_facets_single_line.so OBJECT lib/target_facets_single_line.o INTERFACE include\n",
    );
    let spec = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/target_facets_single_line.DepoFile"),
    )
    .expect("single-line target facets should parse");
    assert_eq!(spec.targets.len(), 1);
    assert_eq!(
        spec.targets[0].static_path,
        Some(PathBuf::from("lib/libtarget_facets_single_line.a"))
    );
    assert_eq!(
        spec.targets[0].shared_path,
        Some(PathBuf::from("lib/libtarget_facets_single_line.so"))
    );
    assert_eq!(
        spec.targets[0].object_path,
        Some(PathBuf::from("lib/target_facets_single_line.o"))
    );
    assert_eq!(spec.targets[0].include_dirs, vec![PathBuf::from("include")]);

    sandbox.write(
        "tmp/target_duplicate_static.DepoFile",
        "NAME dup_static\nVERSION 1.0.0\nTARGET dup_static::dup STATIC lib/libdup_static.a\nTARGET dup_static::dup STATIC lib/libdup_static_again.a\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/target_duplicate_static.DepoFile"),
    )
    .expect_err("duplicate target STATIC facet should be rejected");
    assert!(format!("{error:#}").contains("TARGET dup_static::dup repeats STATIC"));

    sandbox.write(
        "tmp/target_merged_interface.DepoFile",
        "NAME merge_interface\nVERSION 1.0.0\nTARGET merge_interface::merge INTERFACE include\nTARGET merge_interface::merge INTERFACE generated/include\n",
    );
    let spec = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/target_merged_interface.DepoFile"),
    )
    .expect("repeated target INTERFACE facets should merge include dirs");
    assert_eq!(
        spec.targets[0].include_dirs,
        vec![PathBuf::from("include"), PathBuf::from("generated/include")]
    );
}

#[test]
fn depofile_parses_primary_target_and_target_interface_metadata() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/interface_metadata.DepoFile",
        "NAME interface_metadata\nVERSION 1.0.0\nPRIMARY_TARGET interface_metadata::runtime\nTARGET interface_metadata::headers INTERFACE include\nDEFINES interface_metadata::headers BASICS_DEBUG=0 PAGE_SIZE=4096\nDEFINES interface_metadata::headers nametag_log=basics_log\nOPTIONS interface_metadata::headers -Winvalid-pch\nFEATURES interface_metadata::headers cxx_std_20\nTARGET interface_metadata::runtime STATIC lib/libinterface_metadata.a\n",
    );
    let spec = parse_depofile(&sandbox.depos_root().join("tmp/interface_metadata.DepoFile"))
        .expect("primary target and interface metadata should parse");
    assert_eq!(
        spec.primary_target_name.as_deref(),
        Some("interface_metadata::runtime")
    );
    assert_eq!(spec.targets.len(), 2);
    assert_eq!(
        spec.targets[0].compile_definitions,
        vec![
            "BASICS_DEBUG=0".to_string(),
            "PAGE_SIZE=4096".to_string(),
            "nametag_log=basics_log".to_string()
        ]
    );
    assert_eq!(
        spec.targets[0].compile_options,
        vec!["-Winvalid-pch".to_string()]
    );
    assert_eq!(
        spec.targets[0].compile_features,
        vec!["cxx_std_20".to_string()]
    );

    sandbox.write(
        "tmp/unknown_target_defines.DepoFile",
        "NAME unknown_target_defines\nVERSION 1.0.0\nDEFINES unknown_target_defines::missing FOO=1\nTARGET unknown_target_defines::known INTERFACE include\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/unknown_target_defines.DepoFile"),
    )
    .expect_err("DEFINES should reject unknown target names");
    assert!(format!("{error:#}").contains("declares DEFINES for unknown target"));

    sandbox.write(
        "tmp/repeated_primary_target.DepoFile",
        "NAME repeated_primary_target\nVERSION 1.0.0\nPRIMARY_TARGET repeated_primary_target::one\nPRIMARY_TARGET repeated_primary_target::two\nTARGET repeated_primary_target::one INTERFACE include\nTARGET repeated_primary_target::two INTERFACE include\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/repeated_primary_target.DepoFile"),
    )
    .expect_err("PRIMARY_TARGET should reject repeat assignment");
    assert!(format!("{error:#}").contains("PRIMARY_TARGET is already set"));
}

#[test]
fn depofile_rejects_path_escapes_in_declared_paths() {
    let sandbox = Sandbox::new();

    sandbox.write(
        "tmp/source_subdir_escape.DepoFile",
        "NAME source_subdir_escape\nVERSION 1.0.0\nSOURCE GIT /tmp/example HEAD\nSOURCE_SUBDIR ../outside\nARTIFACT include/demo.h\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/source_subdir_escape.DepoFile"),
    )
    .expect_err("SOURCE_SUBDIR path escape should be rejected");
    assert!(format!("{error:#}").contains("SOURCE_SUBDIR"));
    assert!(format!("{error:#}").contains("must not contain '..'"));

    sandbox.write(
        "tmp/artifact_absolute.DepoFile",
        "NAME artifact_absolute\nVERSION 1.0.0\nARTIFACT /tmp/evil\n",
    );
    let error = parse_depofile(&sandbox.depos_root().join("tmp/artifact_absolute.DepoFile"))
        .expect_err("absolute ARTIFACT path should be rejected");
    assert!(format!("{error:#}").contains("ARTIFACT"));
    assert!(format!("{error:#}").contains("must be relative"));

    sandbox.write(
        "tmp/target_path_escape.DepoFile",
        "NAME target_path_escape\nVERSION 1.0.0\nTARGET target_path_escape::core STATIC ../libevil.a INTERFACE include\n",
    );
    let error = parse_depofile(&sandbox.depos_root().join("tmp/target_path_escape.DepoFile"))
        .expect_err("target artifact path escape should be rejected");
    assert!(format!("{error:#}").contains("TARGET target_path_escape::core STATIC path"));
    assert!(format!("{error:#}").contains("must not contain '..'"));

    sandbox.write(
        "tmp/target_include_absolute.DepoFile",
        "NAME target_include_absolute\nVERSION 1.0.0\nTARGET target_include_absolute::core INTERFACE /tmp/include\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/target_include_absolute.DepoFile"),
    )
    .expect_err("absolute target include path should be rejected");
    assert!(format!("{error:#}").contains("TARGET target_include_absolute::core INTERFACE path"));
    assert!(format!("{error:#}").contains("must be relative"));

    sandbox.write(
        "tmp/stage_source_escape.DepoFile",
        "NAME stage_source_escape\nVERSION 1.0.0\nBUILD_SYSTEM MANUAL\nSTAGE_FILE SOURCE ../evil include/demo.h\nTARGET stage_source_escape::demo INTERFACE include\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/stage_source_escape.DepoFile"),
    )
    .expect_err("stage source path escape should be rejected");
    assert!(format!("{error:#}").contains("STAGE_FILE source path"));
    assert!(format!("{error:#}").contains("must not contain '..'"));

    sandbox.write(
        "tmp/stage_destination_absolute.DepoFile",
        "NAME stage_destination_absolute\nVERSION 1.0.0\nBUILD_SYSTEM MANUAL\nSTAGE_TREE SOURCE include /tmp/evil\nTARGET stage_destination_absolute::demo INTERFACE include\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/stage_destination_absolute.DepoFile"),
    )
    .expect_err("absolute stage destination should be rejected");
    assert!(format!("{error:#}").contains("STAGE_TREE destination path"));
    assert!(format!("{error:#}").contains("must be relative"));
}

#[test]
fn depofile_rejects_build_system_directives_from_the_wrong_family() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/wrong_family.DepoFile",
        "NAME wrong_family\nVERSION 1.0.0\nTARGET wrong::wrong STATIC lib/libwrong.a INTERFACE include\nBUILD_SYSTEM CARGO\nCMAKE_CONFIGURE_SH <<EOF\necho nope\nEOF\n",
    );

    let error = parse_depofile(&sandbox.depos_root().join("tmp/wrong_family.DepoFile"))
        .expect_err("wrong-family build-system directive should be rejected");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("CMAKE_CONFIGURE_SH requires BUILD_SYSTEM CMAKE"));
    assert!(rendered.contains("BUILD_SYSTEM CARGO"));
}

#[test]
fn depofile_rejects_build_system_specific_directives_without_build_system() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/missing_build_system.DepoFile",
        "NAME missing_build_system\nVERSION 1.0.0\nTARGET missing::missing STATIC lib/libmissing.a INTERFACE include\nMESON_DEFINE default_library=static\n",
    );

    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/missing_build_system.DepoFile"),
    )
    .expect_err("build-system directive without BUILD_SYSTEM should be rejected");
    assert!(format!("{error:#}").contains("MESON_DEFINE requires BUILD_SYSTEM MESON"));
}

#[test]
fn depofile_rejects_build_system_declared_after_phase_hook() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/late_build_system.DepoFile",
        "NAME late_build_system\nVERSION 1.0.0\nTARGET late::late STATIC lib/liblate.a INTERFACE include\nCMAKE_CONFIGURE_SH <<EOF\necho nope\nEOF\nBUILD_SYSTEM CARGO\n",
    );

    let error = parse_depofile(&sandbox.depos_root().join("tmp/late_build_system.DepoFile"))
        .expect_err("phase hook before BUILD_SYSTEM should be rejected");
    assert!(format!("{error:#}").contains(
        "CMAKE_CONFIGURE_SH requires BUILD_SYSTEM CMAKE and BUILD_SYSTEM must appear before it"
    ));
}

#[test]
fn depofile_rejects_git_submodules_without_preceding_git_source() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/git_submodules_missing_source.DepoFile",
        "NAME git_submodules_missing_source\nVERSION 1.0.0\nGIT_SUBMODULES RECURSIVE\nTARGET git_submodules_missing_source::git_submodules_missing_source INTERFACE include\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/git_submodules_missing_source.DepoFile"),
    )
    .expect_err("GIT_SUBMODULES should require SOURCE GIT");
    assert!(format!("{error:#}").contains("GIT_SUBMODULES requires a preceding SOURCE GIT"));

    sandbox.write(
        "tmp/git_submodules_url.DepoFile",
        "NAME git_submodules_url\nVERSION 1.0.0\nSOURCE URL https://example.invalid/archive.tar.gz\nGIT_SUBMODULES RECURSIVE\nTARGET git_submodules_url::git_submodules_url INTERFACE include\n",
    );
    let error = parse_depofile(&sandbox.depos_root().join("tmp/git_submodules_url.DepoFile"))
        .expect_err("GIT_SUBMODULES should reject SOURCE URL");
    assert!(format!("{error:#}")
        .contains("GIT_SUBMODULES requires a preceding SOURCE GIT, not SOURCE URL"));
}

#[test]
fn checked_local_depofiles_parse_cleanly() {
    let mut depofiles = Vec::new();
    collect_named_files(
        Path::new("/root/depos/depofiles/local"),
        "main.DepoFile",
        &mut depofiles,
    );
    depofiles.sort();
    assert!(!depofiles.is_empty(), "expected checked local DepoFiles");

    for path in depofiles {
        parse_depofile(&path)
            .unwrap_or_else(|error| panic!("{} failed to parse: {error:#}", path.display()));
    }
}

#[test]
fn generated_local_depofiles_are_in_sync_with_canonical_schemas() {
    let status = Command::new("/root/depos/tools/regenerate-local-depofiles.sh")
        .arg("--check")
        .status()
        .expect("run local depofile generator check");
    assert!(
        status.success(),
        "generated local depofiles are out of sync"
    );
}

#[test]
fn depofile_rejects_autoconf_skip_configure_with_explicit_configure_hook() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/autoconf_conflict.DepoFile",
        "NAME autoconf_conflict\nVERSION 1.0.0\nTARGET autoconf_conflict::autoconf_conflict STATIC lib/libautoconf_conflict.a INTERFACE include\nBUILD_SYSTEM AUTOCONF\nAUTOCONF_SKIP_CONFIGURE\nAUTOCONF_CONFIGURE_SH <<EOF\necho nope\nEOF\n",
    );

    let error = parse_depofile(&sandbox.depos_root().join("tmp/autoconf_conflict.DepoFile"))
        .expect_err("AUTOCONF_SKIP_CONFIGURE should conflict with AUTOCONF_CONFIGURE_SH");
    assert!(format!("{error:#}")
        .contains("uses AUTOCONF_SKIP_CONFIGURE together with AUTOCONF_CONFIGURE_SH"));

    sandbox.write(
        "tmp/autoconf_direct_conflict.DepoFile",
        "NAME autoconf_direct_conflict\nVERSION 1.0.0\nTARGET autoconf_direct_conflict::autoconf_direct_conflict STATIC lib/libautoconf_direct_conflict.a INTERFACE include\nBUILD_SYSTEM AUTOCONF\nAUTOCONF_SKIP_CONFIGURE\nAUTOCONF_CONFIGURE ./configure --prefix=${DEPO_PREFIX}\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/autoconf_direct_conflict.DepoFile"),
    )
    .expect_err("AUTOCONF_SKIP_CONFIGURE should conflict with AUTOCONF_CONFIGURE");
    assert!(format!("{error:#}")
        .contains("uses AUTOCONF_SKIP_CONFIGURE together with AUTOCONF_CONFIGURE"));

    sandbox.write(
        "tmp/autoconf_arg_conflict.DepoFile",
        "NAME autoconf_arg_conflict\nVERSION 1.0.0\nTARGET autoconf_arg_conflict::autoconf_arg_conflict STATIC lib/libautoconf_arg_conflict.a INTERFACE include\nBUILD_SYSTEM AUTOCONF\nAUTOCONF_SKIP_CONFIGURE\nAUTOCONF_ARG --disable-shared\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/autoconf_arg_conflict.DepoFile"),
    )
    .expect_err("AUTOCONF_SKIP_CONFIGURE should conflict with AUTOCONF_ARG");
    assert!(
        format!("{error:#}").contains("uses AUTOCONF_SKIP_CONFIGURE together with AUTOCONF_ARG")
    );
}

#[test]
fn depofile_rejects_direct_and_shell_override_for_same_phase() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "tmp/cmake_phase_conflict.DepoFile",
        "NAME cmake_phase_conflict\nVERSION 1.0.0\nTARGET cmake_phase_conflict::cmake_phase_conflict STATIC lib/libcmake_phase_conflict.a INTERFACE include\nBUILD_SYSTEM CMAKE\nCMAKE_BUILD cmake --build . --parallel\nCMAKE_BUILD_SH <<EOF\necho nope\nEOF\n",
    );

    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/cmake_phase_conflict.DepoFile"),
    )
    .expect_err("direct and shell phase overrides should conflict");
    assert!(format!("{error:#}").contains("declares both CMAKE_BUILD and CMAKE_BUILD_SH"));
}

#[test]
fn depofile_rejects_structured_and_explicit_phase_ownership_mix() {
    let sandbox = Sandbox::new();

    sandbox.write(
        "tmp/autoconf_structured_mix.DepoFile",
        "NAME autoconf_structured_mix\nVERSION 1.0.0\nTARGET autoconf_structured_mix::autoconf_structured_mix STATIC lib/libautoconf_structured_mix.a INTERFACE include\nBUILD_SYSTEM AUTOCONF\nAUTOCONF_ARG --disable-shared\nAUTOCONF_CONFIGURE ./configure --prefix=${DEPO_PREFIX} --libdir=${DEPO_PREFIX}/lib --disable-shared\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/autoconf_structured_mix.DepoFile"),
    )
    .expect_err("AUTOCONF_ARG should conflict with AUTOCONF_CONFIGURE");
    assert!(format!("{error:#}")
        .contains("declares both AUTOCONF_CONFIGURE and AUTOCONF_ARG for the same phase"));

    sandbox.write(
        "tmp/cmake_structured_mix.DepoFile",
        "NAME cmake_structured_mix\nVERSION 1.0.0\nTARGET cmake_structured_mix::cmake_structured_mix STATIC lib/libcmake_structured_mix.a INTERFACE include\nBUILD_SYSTEM CMAKE\nCMAKE_DEFINE BUILD_SHARED_LIBS=OFF\nCMAKE_CONFIGURE cmake -S . -B ${DEPO_BUILD_DIR} -G Ninja\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/cmake_structured_mix.DepoFile"),
    )
    .expect_err("CMAKE_DEFINE should conflict with CMAKE_CONFIGURE");
    assert!(format!("{error:#}")
        .contains("declares both CMAKE_CONFIGURE and CMAKE_ARG/CMAKE_DEFINE for the same phase"));

    sandbox.write(
        "tmp/meson_structured_mix.DepoFile",
        "NAME meson_structured_mix\nVERSION 1.0.0\nTARGET meson_structured_mix::meson_structured_mix STATIC lib/libmeson_structured_mix.a INTERFACE include\nBUILD_SYSTEM MESON\nMESON_DEFINE default_library=static\nMESON_SETUP meson setup ${DEPO_BUILD_DIR} ${DEPO_SOURCE_DIR}\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/meson_structured_mix.DepoFile"),
    )
    .expect_err("MESON_DEFINE should conflict with MESON_SETUP");
    assert!(format!("{error:#}")
        .contains("declares both MESON_SETUP and MESON_ARG/MESON_DEFINE for the same phase"));

    sandbox.write(
        "tmp/cargo_structured_mix.DepoFile",
        "NAME cargo_structured_mix\nVERSION 1.0.0\nBUILD_SYSTEM CARGO\nCARGO_BUILD_ARG --manifest-path\nCARGO_BUILD_ARG Cargo.toml\nCARGO_BUILD cargo build --release --target-dir ${DEPO_BUILD_DIR}/cargo-target --manifest-path Cargo.toml\nSTAGE_FILE BUILD cargo-target/release/libcargo_structured_mix.a lib/libcargo_structured_mix.a\nTARGET cargo_structured_mix::cargo_structured_mix STATIC lib/libcargo_structured_mix.a\n",
    );
    let error = parse_depofile(
        &sandbox
            .depos_root()
            .join("tmp/cargo_structured_mix.DepoFile"),
    )
    .expect_err("CARGO_BUILD_ARG should conflict with CARGO_BUILD");
    assert!(format!("{error:#}")
        .contains("declares both CARGO_BUILD and CARGO_BUILD_ARG for the same phase"));
}

#[test]
fn sync_materializes_cmake_build_system_package() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/cmake_demo",
        &[
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.16)\nproject(cmake_demo LANGUAGES C)\nadd_library(cmake_demo STATIC src/demo.c)\ntarget_include_directories(cmake_demo PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)\ninstall(TARGETS cmake_demo ARCHIVE DESTINATION lib)\ninstall(DIRECTORY include/ DESTINATION include)\n",
            ),
            ("src/demo.c", "int cmake_demo_value(void) { return 7; }\n"),
            ("include/cmake_demo/demo.h", "#pragma once\nint cmake_demo_value(void);\n"),
        ],
    );
    sandbox.write(
        "depofiles/local/cmake_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME cmake_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_SYSTEM CMAKE\nSOURCE GIT {} HEAD\nCMAKE_BUILD_SH <<'EOF'\ntest \"$PWD\" = \"$DEPO_BUILD_DIR\"\ncmake --build . --parallel\nEOF\nCMAKE_INSTALL_SH <<'EOF'\ntest \"$PWD\" = \"$DEPO_BUILD_DIR\"\ncmake --install .\nEOF\nTARGET cmake_demo::cmake_demo STATIC lib/libcmake_demo.a INTERFACE include\nCMAKE_DEFINE BUILD_SHARED_LIBS=OFF\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/cmake_demo.cmake",
        "depos_require(cmake_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/cmake_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize BUILD_SYSTEM CMAKE package");

    assert!(sandbox
        .package_store_path(
            "cmake_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/cmake_demo/demo.h",
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "cmake_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "lib/libcmake_demo.a"
        )
        .exists());
}

#[test]
fn sync_materializes_cmake_build_system_package_with_direct_phase_commands() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/cmake_direct_demo",
        &[
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.16)\nproject(cmake_direct_demo LANGUAGES C)\nadd_library(cmake_direct_demo STATIC src/demo.c)\ntarget_include_directories(cmake_direct_demo PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)\ninstall(TARGETS cmake_direct_demo ARCHIVE DESTINATION lib)\ninstall(DIRECTORY include/ DESTINATION include)\n",
            ),
            ("src/demo.c", "int cmake_direct_demo_value(void) { return 17; }\n"),
            ("include/cmake_direct_demo/demo.h", "#pragma once\nint cmake_direct_demo_value(void);\n"),
        ],
    );
    sandbox.write(
        "depofiles/local/cmake_direct_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME cmake_direct_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_SYSTEM CMAKE\nSOURCE GIT {} HEAD\nCMAKE_CONFIGURE cmake -S . -B ${{DEPO_BUILD_DIR}} -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=${{DEPO_PREFIX}} -DCMAKE_INSTALL_LIBDIR=lib\nCMAKE_BUILD cmake --build . --parallel $(nproc)\nCMAKE_INSTALL cmake --install .\nTARGET cmake_direct_demo::cmake_direct_demo STATIC lib/libcmake_direct_demo.a INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/cmake_direct_demo.cmake",
        "depos_require(cmake_direct_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/cmake_direct_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize direct-command CMake package");

    assert!(sandbox
        .package_store_path(
            "cmake_direct_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/cmake_direct_demo/demo.h",
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "cmake_direct_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "lib/libcmake_direct_demo.a",
        )
        .exists());
}

#[test]
fn sync_materializes_meson_build_system_package() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/meson_demo",
        &[
            (
                "meson.build",
                "project('meson_demo', 'c')\ninc = include_directories('include')\nlib = static_library('meson_demo', 'src/demo.c', include_directories: inc, install: true)\ninstall_headers('include/meson_demo/demo.h', subdir: 'meson_demo')\n",
            ),
            ("src/demo.c", "int meson_demo_value(void) { return 9; }\n"),
            ("include/meson_demo/demo.h", "#pragma once\nint meson_demo_value(void);\n"),
        ],
    );
    sandbox.write(
        "depofiles/local/meson_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME meson_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_SYSTEM MESON\nSOURCE GIT {} HEAD\nMESON_COMPILE_SH <<'EOF'\ntest \"$PWD\" = \"$DEPO_BUILD_DIR\"\nmeson compile\nEOF\nMESON_INSTALL_SH <<'EOF'\ntest \"$PWD\" = \"$DEPO_BUILD_DIR\"\nmeson install\nEOF\nTARGET meson_demo::meson_demo STATIC lib/libmeson_demo.a INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/meson_demo.cmake",
        "depos_require(meson_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/meson_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize BUILD_SYSTEM MESON package");

    assert!(sandbox
        .package_store_path(
            "meson_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/meson_demo/demo.h",
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "meson_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "lib/libmeson_demo.a"
        )
        .exists());
}

#[test]
fn sync_materializes_autoconf_build_system_package() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.depos_root().join("upstreams/autoconf_demo");
    fs::create_dir_all(upstream.join("include/autoconf_demo"))
        .expect("create autoconf include dir");
    sandbox.write(
        "upstreams/autoconf_demo/configure",
        "#!/bin/sh\nset -eu\nprefix=/usr/local\nlibdir=\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --prefix=*) prefix=${arg#*=} ;;\n    --libdir=*) libdir=${arg#*=} ;;\n  esac\ndone\n[ -n \"$libdir\" ] || libdir=\"$prefix/lib\"\ncat > config.mk <<EOF\nprefix=$prefix\nlibdir=$libdir\nEOF\n",
    );
    let mut perms = fs::metadata(upstream.join("configure"))
        .expect("stat configure")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(upstream.join("configure"), perms).expect("chmod configure");
    sandbox.write(
        "upstreams/autoconf_demo/Makefile",
        "include config.mk\n\nall:\n\tmkdir -p build\n\tprintf 'archive\\n' > build/libautoconf_demo.a\n\ninstall: all\n\tmkdir -p $(prefix)/include/autoconf_demo $(libdir)\n\tcp include/autoconf_demo/demo.h $(prefix)/include/autoconf_demo/demo.h\n\tcp build/libautoconf_demo.a $(libdir)/libautoconf_demo.a\n",
    );
    sandbox.write(
        "upstreams/autoconf_demo/include/autoconf_demo/demo.h",
        "#pragma once\n",
    );
    run_command(&upstream, ["git", "init", "--quiet"]);
    run_command(
        &upstream,
        ["git", "config", "user.email", "codex@example.invalid"],
    );
    run_command(&upstream, ["git", "config", "user.name", "Codex"]);
    run_command(&upstream, ["git", "add", "."]);
    run_command(&upstream, ["git", "commit", "--quiet", "-m", "init"]);

    sandbox.write(
        "depofiles/local/autoconf_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME autoconf_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_SYSTEM AUTOCONF\nSOURCE GIT {} HEAD\nAUTOCONF_CONFIGURE_SH <<'EOF'\ntest \"$PWD\" = \"$DEPO_SOURCE_DIR\"\ntest \"$CC\" = clang\ntest \"$CXX\" = clang++\ntest \"$AR\" = ar\ntest \"$RANLIB\" = ranlib\ntest \"$STRIP\" = strip\n./configure --prefix=\"$DEPO_PREFIX\" --libdir=\"$DEPO_PREFIX/lib\"\nEOF\nTARGET autoconf_demo::autoconf_demo STATIC lib/libautoconf_demo.a INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/autoconf_demo.cmake",
        "depos_require(autoconf_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/autoconf_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize BUILD_SYSTEM AUTOCONF package");

    assert!(sandbox
        .package_store_path(
            "autoconf_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/autoconf_demo/demo.h",
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "autoconf_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "lib/libautoconf_demo.a",
        )
        .exists());
}

#[test]
fn sync_injects_pkg_config_libdir_for_autoconf_dependencies() {
    let sandbox = Sandbox::new();
    let dependency = sandbox.create_git_repo(
        "upstreams/dependency_demo",
        &[
            ("include/dependency_demo/demo.h", "#pragma once\n"),
            ("lib/libdependency_demo.a", "archive\n"),
        ],
    );
    let upstream = sandbox
        .depos_root()
        .join("upstreams/autoconf_pkg_config_demo");
    fs::create_dir_all(upstream.join("include/autoconf_pkg_config_demo"))
        .expect("create pkg-config demo include dir");
    sandbox.write(
        "upstreams/autoconf_pkg_config_demo/configure",
        "#!/bin/sh\nset -eu\nprefix=/usr/local\nlibdir=\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --prefix=*) prefix=${arg#*=} ;;\n    --libdir=*) libdir=${arg#*=} ;;\n  esac\ndone\n[ -n \"$libdir\" ] || libdir=\"$prefix/lib\"\ncat > config.mk <<EOF\nprefix=$prefix\nlibdir=$libdir\nEOF\n",
    );
    let mut perms = fs::metadata(upstream.join("configure"))
        .expect("stat pkg-config demo configure")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(upstream.join("configure"), perms).expect("chmod pkg-config demo");
    sandbox.write(
        "upstreams/autoconf_pkg_config_demo/Makefile",
        "include config.mk\n\nall:\n\tmkdir -p build\n\tprintf 'archive\\n' > build/libautoconf_pkg_config_demo.a\n\ninstall: all\n\tmkdir -p $(prefix)/include/autoconf_pkg_config_demo $(libdir)\n\tcp include/autoconf_pkg_config_demo/demo.h $(prefix)/include/autoconf_pkg_config_demo/demo.h\n\tcp build/libautoconf_pkg_config_demo.a $(libdir)/libautoconf_pkg_config_demo.a\n",
    );
    sandbox.write(
        "upstreams/autoconf_pkg_config_demo/include/autoconf_pkg_config_demo/demo.h",
        "#pragma once\n",
    );
    run_command(&upstream, ["git", "init", "--quiet"]);
    run_command(
        &upstream,
        ["git", "config", "user.email", "codex@example.invalid"],
    );
    run_command(&upstream, ["git", "config", "user.name", "Codex"]);
    run_command(&upstream, ["git", "add", "."]);
    run_command(&upstream, ["git", "commit", "--quiet", "-m", "init"]);

    sandbox.write(
        "depofiles/local/dependency_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME dependency_demo\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nTARGET dependency_demo::dependency_demo INTERFACE include\nARTIFACT lib/libdependency_demo.a\n",
            dependency.display()
        ),
    );
    sandbox.write(
        "depofiles/local/autoconf_pkg_config_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME autoconf_pkg_config_demo\nVERSION 1.0.0\nSYSTEM_LIBS ALLOW\nDEPENDS dependency_demo VERSION 1.0.0\nBUILD_SYSTEM AUTOCONF\nSOURCE GIT {} HEAD\nAUTOCONF_CONFIGURE_SH <<'EOF'\nexpected=\"/depos/dependency_demo/release/1.0.0/lib/pkgconfig:/depos/dependency_demo/release/1.0.0/lib64/pkgconfig\"\ntest \"$PKG_CONFIG_LIBDIR\" = \"$expected\"\n./configure --prefix=\"$DEPO_PREFIX\" --libdir=\"$DEPO_PREFIX/lib\"\nEOF\nTARGET autoconf_pkg_config_demo::autoconf_pkg_config_demo STATIC lib/libautoconf_pkg_config_demo.a INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/autoconf_pkg_config_demo.cmake",
        "depos_require(autoconf_pkg_config_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/autoconf_pkg_config_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should inject dependency PKG_CONFIG_LIBDIR automatically");

    assert!(sandbox
        .package_store_path(
            "autoconf_pkg_config_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/autoconf_pkg_config_demo/demo.h",
        )
        .exists());
}

#[test]
fn sync_materializes_autoconf_direct_configure_relative_executable_package() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.depos_root().join("upstreams/autoconf_direct_demo");
    fs::create_dir_all(upstream.join("include/autoconf_direct_demo"))
        .expect("create autoconf direct include dir");
    sandbox.write(
        "upstreams/autoconf_direct_demo/configure",
        "#!/bin/sh\nset -eu\nprefix=/usr/local\nlibdir=\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --prefix=*) prefix=${arg#*=} ;;\n    --libdir=*) libdir=${arg#*=} ;;\n  esac\ndone\n[ -n \"$libdir\" ] || libdir=\"$prefix/lib\"\ncat > config.mk <<EOF\nprefix=$prefix\nlibdir=$libdir\nEOF\n",
    );
    let mut perms = fs::metadata(upstream.join("configure"))
        .expect("stat direct configure")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(upstream.join("configure"), perms).expect("chmod direct configure");
    sandbox.write(
        "upstreams/autoconf_direct_demo/Makefile",
        "include config.mk\n\nall:\n\tmkdir -p build\n\tprintf 'archive\\n' > build/libautoconf_direct_demo.a\n\ninstall: all\n\tmkdir -p $(prefix)/include/autoconf_direct_demo $(libdir)\n\tcp include/autoconf_direct_demo/demo.h $(prefix)/include/autoconf_direct_demo/demo.h\n\tcp build/libautoconf_direct_demo.a $(libdir)/libautoconf_direct_demo.a\n",
    );
    sandbox.write(
        "upstreams/autoconf_direct_demo/include/autoconf_direct_demo/demo.h",
        "#pragma once\n",
    );
    run_command(&upstream, ["git", "init", "--quiet"]);
    run_command(
        &upstream,
        ["git", "config", "user.email", "codex@example.invalid"],
    );
    run_command(&upstream, ["git", "config", "user.name", "Codex"]);
    run_command(&upstream, ["git", "add", "."]);
    run_command(&upstream, ["git", "commit", "--quiet", "-m", "init"]);

    sandbox.write(
        "depofiles/local/autoconf_direct_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME autoconf_direct_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_SYSTEM AUTOCONF\nSOURCE GIT {} HEAD\nAUTOCONF_CONFIGURE ./configure --prefix=\"${{DEPO_PREFIX}}\" --libdir=\"${{DEPO_PREFIX}}/lib\"\nTARGET autoconf_direct_demo::autoconf_direct_demo STATIC lib/libautoconf_direct_demo.a INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/autoconf_direct_demo.cmake",
        "depos_require(autoconf_direct_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/autoconf_direct_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize AUTOCONF_CONFIGURE ./configure package");

    assert!(sandbox
        .package_store_path(
            "autoconf_direct_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/autoconf_direct_demo/demo.h",
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "autoconf_direct_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "lib/libautoconf_direct_demo.a",
        )
        .exists());
}

#[test]
fn sync_materializes_cargo_build_system_package() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/cargo_demo",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"cargo_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"staticlib\"]\n",
            ),
            ("src/lib.rs", "#[no_mangle]\npub extern \"C\" fn cargo_demo_value() -> i32 { 11 }\n"),
        ],
    );
    sandbox.write(
        "depofiles/local/cargo_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME cargo_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_SYSTEM CARGO\nSOURCE GIT {} HEAD\nCARGO_BUILD_SH <<'EOF'\ntest \"$PWD\" = \"$DEPO_SOURCE_DIR\"\ntest \"$CC\" = clang\ntest \"$CARGO_HOME\" = \"$DEPO_BUILD_DIR/cargo-home\"\ncargo build --release --target-dir \"$DEPO_BUILD_DIR/cargo-target\"\nEOF\nSTAGE_FILE BUILD cargo-target/release/libcargo_demo.a lib/libcargo_demo.a\nTARGET cargo_demo::cargo_demo STATIC lib/libcargo_demo.a\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/cargo_demo.cmake",
        "depos_require(cargo_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/cargo_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize BUILD_SYSTEM CARGO package");

    assert!(sandbox
        .package_store_path(
            "cargo_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "lib/libcargo_demo.a"
        )
        .exists());
}

#[test]
fn sync_emits_target_specific_link_libraries() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/link_demo",
        &[
            ("payload/demo.h", "// link demo\n"),
            ("payload/liblink_demo.a", "archive\n"),
        ],
    );
    sandbox.write(
        "depofiles/local/link_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME link_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET link_demo::aggregate INTERFACE include\nTARGET link_demo::core STATIC lib/liblink_demo.a\nLINK link_demo::aggregate link_demo::core::static m pthread\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/link_demo\" \"${{DEPO_PREFIX}}/lib\" && cp \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/link_demo/demo.h\" && cp \"${{DEPO_SOURCE_DIR}}/payload/liblink_demo.a\" \"${{DEPO_PREFIX}}/lib/liblink_demo.a\"\nEOF\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/link_demo.cmake",
        "depos_require(link_demo VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/link_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should emit target-specific links");

    let targets = fs::read_to_string(output.targets_file).expect("targets.cmake should exist");
    assert!(targets
        .contains("add_library(link_demo::aggregate ALIAS _depos_link_demo_release_1_0_0_t0)"));
    assert!(targets.contains(
        "add_library(link_demo::aggregate::interface ALIAS _depos_link_demo_release_1_0_0_t0__interface)"
    ));
    assert!(
        targets.contains("add_library(link_demo::core ALIAS _depos_link_demo_release_1_0_0_t1)")
    );
    assert!(targets.contains(
        "add_library(link_demo::core::static ALIAS _depos_link_demo_release_1_0_0_t1__static)"
    ));
    assert!(targets.contains(
        "add_library(link_demo::core::interface ALIAS _depos_link_demo_release_1_0_0_t1__interface)"
    ));
    assert!(targets.contains("INTERFACE_INCLUDE_DIRECTORIES"));
    assert!(targets.contains("/link_demo/release/1.0.0/include"));
    assert!(targets.contains(
        "INTERFACE_LINK_LIBRARIES \"_depos_link_demo_release_1_0_0_t1__static;m;pthread\""
    ));
    assert!(targets
        .contains("INTERFACE_LINK_LIBRARIES \"_depos_link_demo_release_1_0_0_t1__interface\""));
    assert_eq!(
        targets.matches("INTERFACE_INCLUDE_DIRECTORIES").count(),
        1,
        "{targets}"
    );
}

#[test]
fn sync_emits_generated_aliases_for_multi_role_target() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/multi_role_demo",
        &[
            ("include/multi_role_demo/demo.h", "// multi role demo\n"),
            ("lib/libmulti_role_demo.a", "archive\n"),
            ("lib/libmulti_role_demo.so", "shared\n"),
        ],
    );
    sandbox.write(
        "depofiles/local/multi_role_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME multi_role_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nTARGET multi_role_demo::multi_role_demo STATIC lib/libmulti_role_demo.a SHARED lib/libmulti_role_demo.so INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/multi_role_demo.cmake",
        "depos_require(multi_role_demo VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/multi_role_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should emit generated multi-role aliases");

    let targets = fs::read_to_string(output.targets_file).expect("targets.cmake should exist");
    assert!(targets.contains(
        "add_library(multi_role_demo::multi_role_demo ALIAS _depos_multi_role_demo_release_1_0_0_t0)"
    ));
    assert!(targets.contains(
        "add_library(multi_role_demo::multi_role_demo::interface ALIAS _depos_multi_role_demo_release_1_0_0_t0__interface)"
    ));
    assert!(targets.contains(
        "add_library(multi_role_demo::multi_role_demo::static ALIAS _depos_multi_role_demo_release_1_0_0_t0__static)"
    ));
    assert!(targets.contains(
        "add_library(multi_role_demo::multi_role_demo::shared ALIAS _depos_multi_role_demo_release_1_0_0_t0__shared)"
    ));
    assert!(targets.contains(
        "INTERFACE_LINK_LIBRARIES \"_depos_multi_role_demo_release_1_0_0_t0__interface\""
    ));
}

#[test]
fn sync_materializes_local_git_package_into_fresh_root() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/local_itoa",
        &[("include/itoa/jeaiii_to_text.h", "// from git\n")],
    );
    sandbox.write(
        "depofiles/local/local_itoa/release/main/main.DepoFile",
        &format!(
            "NAME local_itoa\nVERSION main\nSYSTEM_LIBS INHERIT\nTARGET itoa::itoa INTERFACE include\nSOURCE GIT {} HEAD\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/local_itoa.cmake",
        "depos_require(local_itoa VERSION main)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/local_itoa.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize local git package");

    assert!(sandbox
        .package_store_path(
            "local_itoa",
            RELEASE_NAMESPACE,
            "main",
            "include/itoa/jeaiii_to_text.h"
        )
        .exists());
    let targets = fs::read_to_string(output.targets_file).expect("targets should exist");
    assert!(targets.contains("add_library(itoa::itoa ALIAS _depos_local_itoa_release_main_t0)"));

    let statuses = collect_statuses(&StatusOptions {
        depos_root: sandbox.depos_root(),
        name: Some("local_itoa".to_string()),
        namespace: None,
        version: Some("main".to_string()),
        refresh: false,
    })
    .expect("status should succeed");
    assert_eq!(statuses[0].state, PackageState::Green);
}

#[test]
fn sync_materializes_local_packages_in_dependency_order() {
    let sandbox = Sandbox::new();
    let base_upstream =
        sandbox.create_git_repo("upstreams/base_dep", &[("payload/base.h", "// base\n")]);
    let dependent_upstream = sandbox.create_git_repo(
        "upstreams/dependent_dep",
        &[("payload/dependent.h", "// dependent\n")],
    );
    sandbox.write(
        "depofiles/local/base/release/1.0.0/main.DepoFile",
        &format!(
            "NAME base\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET base::base INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/base\" && cp \"${{DEPO_SOURCE_DIR}}/payload/base.h\" \"${{DEPO_PREFIX}}/include/base/base.h\"\nEOF\n",
            base_upstream.display()
        ),
    );
    sandbox.write(
        "depofiles/local/dependent/release/1.0.0/main.DepoFile",
        &format!(
            "NAME dependent\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nDEPENDS base VERSION 1.0.0\nTARGET dependent::dependent INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ntest -f \"$DEPO_DEP_BASE_RELEASE_ROOT/include/base/base.h\" && mkdir -p \"${{DEPO_PREFIX}}/include/dependent\" && cp \"${{DEPO_SOURCE_DIR}}/payload/dependent.h\" \"${{DEPO_PREFIX}}/include/dependent/dependent.h\"\nEOF\n",
            dependent_upstream.display()
        ),
    );
    sandbox.write(
        "manifests/dependent_first.cmake",
        "depos_require(dependent VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/dependent_first.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize local dependencies before dependents");

    assert!(output
        .selected
        .iter()
        .any(|package| package.spec.package_id() == "base[release]@1.0.0"));
    assert!(output
        .selected
        .iter()
        .any(|package| package.spec.package_id() == "dependent[release]@1.0.0"));
    assert!(sandbox
        .package_store_path("base", RELEASE_NAMESPACE, "1.0.0", "include/base/base.h")
        .exists());
    assert!(sandbox
        .package_store_path(
            "dependent",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/dependent/dependent.h",
        )
        .exists());
}

#[test]
fn sync_rematerializes_local_dependents_when_dependency_materialization_changes() {
    let sandbox = Sandbox::new();
    let base_upstream = sandbox.create_git_repo(
        "upstreams/base_snapshot",
        &[("payload/base.h", "// first base\n")],
    );
    let dependent_upstream = sandbox.create_git_repo(
        "upstreams/dependent_snapshot",
        &[("payload/unused.txt", "unused\n")],
    );
    sandbox.write(
        "depofiles/local/base_snapshot/release/1.0.0/main.DepoFile",
        &format!(
            "NAME base_snapshot\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET base_snapshot::base_snapshot INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/base_snapshot\" && cp \"${{DEPO_SOURCE_DIR}}/payload/base.h\" \"${{DEPO_PREFIX}}/include/base_snapshot/base.h\"\nEOF\n",
            base_upstream.display()
        ),
    );
    sandbox.write(
        "depofiles/local/dependent_snapshot/release/1.0.0/main.DepoFile",
        &format!(
            "NAME dependent_snapshot\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nDEPENDS base_snapshot VERSION 1.0.0\nTARGET dependent_snapshot::dependent_snapshot INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/dependent_snapshot\" && cp \"$DEPO_DEP_BASE_SNAPSHOT_RELEASE_ROOT/include/base_snapshot/base.h\" \"${{DEPO_PREFIX}}/include/dependent_snapshot/base_snapshot.h\"\nEOF\n",
            dependent_upstream.display()
        ),
    );
    sandbox.write(
        "manifests/dependent_snapshot.cmake",
        "depos_require(dependent_snapshot VERSION 1.0.0)\n",
    );

    let options = SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/dependent_snapshot.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    };
    sync_registry(&options).expect("first sync should materialize dependency snapshot");
    let dependent_header = sandbox.package_store_path(
        "dependent_snapshot",
        RELEASE_NAMESPACE,
        "1.0.0",
        "include/dependent_snapshot/base_snapshot.h",
    );
    assert_eq!(
        fs::read_to_string(&dependent_header).expect("read first dependent snapshot"),
        "// first base\n"
    );

    fs::write(base_upstream.join("payload/base.h"), "// second base\n")
        .expect("update base snapshot header");
    run_command(&base_upstream, ["git", "add", "payload/base.h"]);
    run_command(
        &base_upstream,
        ["git", "commit", "--quiet", "-m", "update base snapshot"],
    );

    sync_registry(&options).expect("second sync should rematerialize dependent snapshot");

    assert_eq!(
        fs::read_to_string(&dependent_header).expect("read second dependent snapshot"),
        "// second base\n"
    );
    let log = fs::read_to_string(sandbox.package_log_path(
        "dependent_snapshot",
        RELEASE_NAMESPACE,
        "1.0.0",
    ))
    .expect("read dependent snapshot log");
    assert!(!log.contains("materialization already up to date"));
    assert!(log.contains("materialization complete"));
}

#[test]
fn sync_materializes_independent_local_packages_in_parallel() {
    if std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }

    let sandbox = Sandbox::new();
    for name in ["alpha_parallel", "beta_parallel", "gamma_parallel"] {
        let upstream = sandbox.create_git_repo(
            &format!("upstreams/{name}"),
            &[(&format!("payload/{name}.h"), &format!("// {name}\n"))],
        );
        sandbox.write(
            &format!("depofiles/local/{name}/release/1.0.0/main.DepoFile"),
            &format!(
                "NAME {name}\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET {name}::{name} INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nsleep 2\nmkdir -p \"${{DEPO_PREFIX}}/include/{name}\" && cp \"${{DEPO_SOURCE_DIR}}/payload/{name}.h\" \"${{DEPO_PREFIX}}/include/{name}/{name}.h\"\nEOF\n",
                upstream.display()
            ),
        );
    }
    sandbox.write(
        "manifests/parallel_locals.cmake",
        "depos_require(alpha_parallel VERSION 1.0.0)\ndepos_require(beta_parallel VERSION 1.0.0)\ndepos_require(gamma_parallel VERSION 1.0.0)\n",
    );

    let started = Instant::now();
    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/parallel_locals.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize independent local packages in parallel");
    let elapsed = started.elapsed();

    assert!(
        elapsed.as_secs_f32() < 5.0,
        "expected parallel local materialization, elapsed {:?}",
        elapsed
    );
    for name in ["alpha_parallel", "beta_parallel", "gamma_parallel"] {
        assert!(sandbox
            .package_store_path(
                name,
                RELEASE_NAMESPACE,
                "1.0.0",
                &format!("include/{name}/{name}.h"),
            )
            .exists());
    }
}

#[test]
fn sync_inherits_dependency_namespace_and_interpolates_dep_roots() {
    let sandbox = Sandbox::new();
    let base_upstream = sandbox.create_git_repo(
        "upstreams/base_dev",
        &[("include/base/base.h", "// base dev namespace\n")],
    );
    let dependent_upstream = sandbox.create_git_repo(
        "upstreams/dependent_dev",
        &[(
            "include/dependent/dependent.h",
            "// dependent dev namespace\n",
        )],
    );
    sandbox.write(
        "depofiles/local/base/dev-main/1.0.0/main.DepoFile",
        &format!(
            "NAME base\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nTARGET base::base INTERFACE include\n",
            base_upstream.display()
        ),
    );
    sandbox.write(
        "depofiles/local/dependent/dev-main/1.0.0/main.DepoFile",
        &format!(
            "NAME dependent\nVERSION 1.0.0\nDEPENDS base VERSION 1.0.0\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD /usr/bin/test -f ${{dep:base}}/include/base/base.h\nTARGET dependent::dependent INTERFACE include\n",
            dependent_upstream.display()
        ),
    );
    sandbox.write(
        "manifests/dependent_dev_namespace.cmake",
        "depos_require(dependent NAMESPACE dev-main VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/dependent_dev_namespace.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should inherit dependency namespace and interpolate dep roots");

    assert!(sandbox
        .package_store_path("base", "dev-main", "1.0.0", "include/base/base.h")
        .exists());
    assert!(sandbox
        .package_store_path(
            "dependent",
            "dev-main",
            "1.0.0",
            "include/dependent/dependent.h",
        )
        .exists());
}

#[test]
fn sync_preserves_symlinked_exports_in_store() {
    let sandbox = Sandbox::new();
    let repo = sandbox.depos_root().join("upstreams/symlink_demo");
    fs::create_dir_all(repo.join("include/symlink_demo")).expect("create include dir");
    fs::create_dir_all(repo.join("lib")).expect("create lib dir");
    fs::write(
        repo.join("include/symlink_demo/real.h"),
        "// symlink header target\n",
    )
    .expect("write target header");
    fs::write(repo.join("lib/libsymlink_demo.so.1"), "shared-object\n")
        .expect("write shared object");
    symlink("real.h", repo.join("include/symlink_demo/current.h")).expect("create header symlink");
    symlink("libsymlink_demo.so.1", repo.join("lib/libsymlink_demo.so"))
        .expect("create shared object symlink");
    run_command(&repo, ["git", "init", "--quiet"]);
    run_command(
        &repo,
        ["git", "config", "user.email", "codex@example.invalid"],
    );
    run_command(&repo, ["git", "config", "user.name", "Codex"]);
    run_command(&repo, ["git", "add", "."]);
    run_command(&repo, ["git", "commit", "--quiet", "-m", "init"]);

    sandbox.write(
        "depofiles/local/symlink_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME symlink_demo\nVERSION 1.0.0\nTARGET symlink_demo::symlink_demo SHARED lib/libsymlink_demo.so INTERFACE include\nARTIFACT lib/libsymlink_demo.so.1\nSOURCE GIT {} HEAD\n",
            repo.display()
        ),
    );
    sandbox.write(
        "manifests/symlink_demo.cmake",
        "depos_require(symlink_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/symlink_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should preserve symlinked exports");

    let header_symlink = sandbox.package_store_path(
        "symlink_demo",
        RELEASE_NAMESPACE,
        "1.0.0",
        "include/symlink_demo/current.h",
    );
    let library_symlink = sandbox.package_store_path(
        "symlink_demo",
        RELEASE_NAMESPACE,
        "1.0.0",
        "lib/libsymlink_demo.so",
    );
    assert!(fs::symlink_metadata(&header_symlink)
        .expect("stat exported header symlink")
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_link(&header_symlink).expect("read exported header symlink"),
        PathBuf::from("real.h")
    );
    assert!(fs::symlink_metadata(&library_symlink)
        .expect("stat exported library symlink")
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_link(&library_symlink).expect("read exported library symlink"),
        PathBuf::from("libsymlink_demo.so.1")
    );
}

#[test]
fn sync_rejects_exported_symlink_that_escapes_source_root() {
    let sandbox = Sandbox::new();
    let repo = sandbox.depos_root().join("upstreams/symlink_escape_demo");
    fs::create_dir_all(repo.join("include/symlink_escape_demo")).expect("create include dir");
    symlink(
        "/etc/passwd",
        repo.join("include/symlink_escape_demo/current.h"),
    )
    .expect("create escaping symlink");
    run_command(&repo, ["git", "init", "--quiet"]);
    run_command(
        &repo,
        ["git", "config", "user.email", "codex@example.invalid"],
    );
    run_command(&repo, ["git", "config", "user.name", "Codex"]);
    run_command(&repo, ["git", "add", "."]);
    run_command(&repo, ["git", "commit", "--quiet", "-m", "init"]);

    sandbox.write(
        "depofiles/local/symlink_escape_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME symlink_escape_demo\nVERSION 1.0.0\nTARGET symlink_escape_demo::symlink_escape_demo INTERFACE include\nSOURCE GIT {} HEAD\n",
            repo.display()
        ),
    );
    sandbox.write(
        "manifests/symlink_escape_demo.cmake",
        "depos_require(symlink_escape_demo VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/symlink_escape_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("escaping symlink exports should be rejected");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("points outside its allowed root"));
    assert!(rendered.contains("current.h"));
}

#[test]
fn sync_rejects_source_subdir_symlink_that_escapes_source_root() {
    let sandbox = Sandbox::new();
    let escaped_dir = sandbox.depos_root().join("tmp/escaped-source-subdir");
    fs::create_dir_all(escaped_dir.join("include/source_subdir_escape"))
        .expect("create escaped source_subdir dir");
    fs::write(
        escaped_dir.join("include/source_subdir_escape/demo.h"),
        "#pragma once\n",
    )
    .expect("write escaped source_subdir header");

    let repo = sandbox.create_git_repo(
        "upstreams/source_subdir_escape",
        &[("README.md", "source subdir escape\n")],
    );
    symlink(&escaped_dir, repo.join("escaped")).expect("create source_subdir escape symlink");
    run_command(&repo, ["git", "add", "escaped"]);
    run_command(
        &repo,
        [
            "git",
            "commit",
            "--quiet",
            "-m",
            "add escaping source_subdir",
        ],
    );

    sandbox.write(
        "depofiles/local/source_subdir_escape/release/1.0.0/main.DepoFile",
        &format!(
            "NAME source_subdir_escape\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nSOURCE_SUBDIR escaped\nTARGET source_subdir_escape::source_subdir_escape INTERFACE include\n",
            repo.display()
        ),
    );
    sandbox.write(
        "manifests/source_subdir_escape.cmake",
        "depos_require(source_subdir_escape VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/source_subdir_escape.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("SOURCE_SUBDIR symlink escape should be rejected");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("SOURCE_SUBDIR"), "{rendered}");
    assert!(
        rendered.contains("resolves outside its allowed root"),
        "{rendered}"
    );
}

#[test]
fn sync_rejects_staged_symlink_that_escapes_source_root() {
    let sandbox = Sandbox::new();
    let repo = sandbox
        .depos_root()
        .join("upstreams/staged_symlink_escape_demo");
    fs::create_dir_all(repo.join("payload")).expect("create payload dir");
    symlink("/etc/passwd", repo.join("payload/escape.h")).expect("create staged escape symlink");
    run_command(&repo, ["git", "init", "--quiet"]);
    run_command(
        &repo,
        ["git", "config", "user.email", "codex@example.invalid"],
    );
    run_command(&repo, ["git", "config", "user.name", "Codex"]);
    run_command(&repo, ["git", "add", "."]);
    run_command(&repo, ["git", "commit", "--quiet", "-m", "init"]);

    sandbox.write(
        "depofiles/local/staged_symlink_escape_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME staged_symlink_escape_demo\nVERSION 1.0.0\nBUILD_SYSTEM MANUAL\nSTAGE_FILE SOURCE payload/escape.h include/staged_symlink_escape_demo/escape.h\nTARGET staged_symlink_escape_demo::staged_symlink_escape_demo INTERFACE include\nSOURCE GIT {} HEAD\n",
            repo.display()
        ),
    );
    sandbox.write(
        "manifests/staged_symlink_escape_demo.cmake",
        "depos_require(staged_symlink_escape_demo VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/staged_symlink_escape_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("staged escaping symlink should be rejected");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("resolves outside its allowed root"));
    assert!(rendered.contains("escape.h"));
}

#[test]
fn sync_rejects_export_path_through_intermediate_symlink_directory() {
    let sandbox = Sandbox::new();
    let escaped_dir = sandbox.depos_root().join("tmp/escaped-export-directory");
    fs::create_dir_all(&escaped_dir).expect("create escaped export dir");
    fs::write(escaped_dir.join("demo.h"), "#pragma once\n").expect("write escaped export file");

    let repo = sandbox.create_git_repo(
        "upstreams/export_path_escape",
        &[("README.md", "export path escape\n")],
    );
    symlink(&escaped_dir, repo.join("escape")).expect("create export escape symlink");
    run_command(&repo, ["git", "add", "escape"]);
    run_command(
        &repo,
        [
            "git",
            "commit",
            "--quiet",
            "-m",
            "add escaping export directory",
        ],
    );

    sandbox.write(
        "depofiles/local/export_path_escape/release/1.0.0/main.DepoFile",
        &format!(
            "NAME export_path_escape\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nARTIFACT escape/demo.h\n",
            repo.display()
        ),
    );
    sandbox.write(
        "manifests/export_path_escape.cmake",
        "depos_require(export_path_escape VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/export_path_escape.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("intermediate export symlink directory should be rejected");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("declared export"), "{rendered}");
    assert!(
        rendered.contains("resolves outside its allowed root"),
        "{rendered}"
    );
}

#[test]
fn sync_rejects_stage_source_path_through_intermediate_symlink_directory() {
    let sandbox = Sandbox::new();
    let escaped_dir = sandbox.depos_root().join("tmp/escaped-stage-directory");
    fs::create_dir_all(&escaped_dir).expect("create escaped stage dir");
    fs::write(escaped_dir.join("demo.h"), "#pragma once\n").expect("write escaped stage file");

    let repo = sandbox.create_git_repo(
        "upstreams/stage_path_escape",
        &[("README.md", "stage path escape\n")],
    );
    symlink(&escaped_dir, repo.join("escape")).expect("create stage escape symlink");
    run_command(&repo, ["git", "add", "escape"]);
    run_command(
        &repo,
        [
            "git",
            "commit",
            "--quiet",
            "-m",
            "add escaping stage directory",
        ],
    );

    sandbox.write(
        "depofiles/local/stage_path_escape/release/1.0.0/main.DepoFile",
        &format!(
            "NAME stage_path_escape\nVERSION 1.0.0\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nSTAGE_FILE SOURCE escape/demo.h include/stage_path_escape/demo.h\nTARGET stage_path_escape::stage_path_escape INTERFACE include\n",
            repo.display()
        ),
    );
    sandbox.write(
        "manifests/stage_path_escape.cmake",
        "depos_require(stage_path_escape VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/stage_path_escape.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("intermediate stage source symlink directory should be rejected");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("manual install source"), "{rendered}");
    assert!(
        rendered.contains("resolves outside its allowed root"),
        "{rendered}"
    );
}

#[test]
fn sync_replaces_stale_owned_exports_and_unregister_cleans_store() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/owned_demo",
        &[("include/owned_demo/one.h", "// first export\n")],
    );
    sandbox.write(
        "depofiles/local/owned_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME owned_demo\nVERSION 1.0.0\nTARGET owned_demo::owned_demo INTERFACE include\nSOURCE GIT {} HEAD\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/owned_demo.cmake",
        "depos_require(owned_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/owned_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("first sync should materialize owned exports");

    let variant = default_variant();
    let first_header = sandbox.package_store_path_for_variant(
        &variant,
        "owned_demo",
        RELEASE_NAMESPACE,
        "1.0.0",
        "include/owned_demo/one.h",
    );
    let second_header = sandbox.package_store_path_for_variant(
        &variant,
        "owned_demo",
        RELEASE_NAMESPACE,
        "1.0.0",
        "include/owned_demo/two.h",
    );
    assert!(first_header.exists());

    fs::remove_file(upstream.join("include/owned_demo/one.h")).expect("remove first header");
    fs::write(
        upstream.join("include/owned_demo/two.h"),
        "// second export\n",
    )
    .expect("write second header");
    run_command(&upstream, ["git", "add", "-A"]);
    run_command(
        &upstream,
        ["git", "commit", "--quiet", "-m", "update exports"],
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/owned_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("second sync should replace stale owned exports");

    assert!(!first_header.exists());
    assert_eq!(
        fs::read_to_string(&second_header).expect("read second header"),
        "// second export\n"
    );

    sandbox.write_store("include/unrelated/keep.h", "// keep me\n");
    unregister_depofile(&UnregisterOptions {
        depos_root: sandbox.depos_root(),
        name: "owned_demo".to_string(),
        namespace: RELEASE_NAMESPACE.to_string(),
        version: "1.0.0".to_string(),
    })
    .expect("unregister should remove owned exports");

    assert!(!second_header.exists());
    assert!(sandbox
        .depos_root()
        .join("store")
        .join(&variant)
        .join("include/unrelated/keep.h")
        .exists());
    assert!(!sandbox
        .depos_root()
        .join(".run/exports/owned_demo/release/1.0.0.exports")
        .exists());
}

#[test]
fn sync_rematerializes_when_the_depofile_changes() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/depofile_cache_demo",
        &[
            ("payload/one.h", "// from one\n"),
            ("payload/two.h", "// from two\n"),
        ],
    );
    let pinned_commit = run_command_capture(&upstream, ["git", "rev-parse", "HEAD"]);
    let depofile_path = "depofiles/local/depofile_cache_demo/release/1.0.0/main.DepoFile";
    sandbox.write(
        depofile_path,
        &format!(
            "NAME depofile_cache_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET depofile_cache_demo::depofile_cache_demo INTERFACE include\nSOURCE GIT {} {}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/depofile_cache_demo\" && cp \"${{DEPO_SOURCE_DIR}}/payload/one.h\" \"${{DEPO_PREFIX}}/include/depofile_cache_demo/demo.h\"\nEOF\n",
            upstream.display(),
            pinned_commit.trim()
        ),
    );
    sandbox.write(
        "manifests/depofile_cache_demo.cmake",
        "depos_require(depofile_cache_demo VERSION 1.0.0)\n",
    );

    let options = SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/depofile_cache_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    };
    let installed_header = sandbox.package_store_path(
        "depofile_cache_demo",
        RELEASE_NAMESPACE,
        "1.0.0",
        "include/depofile_cache_demo/demo.h",
    );

    sync_registry(&options).expect("first sync should materialize depofile cache demo");
    assert_eq!(
        fs::read_to_string(&installed_header).expect("read first depofile cache header"),
        "// from one\n"
    );

    sandbox.write(
        depofile_path,
        &format!(
            "NAME depofile_cache_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET depofile_cache_demo::depofile_cache_demo INTERFACE include\nSOURCE GIT {} {}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/depofile_cache_demo\" && cp \"${{DEPO_SOURCE_DIR}}/payload/two.h\" \"${{DEPO_PREFIX}}/include/depofile_cache_demo/demo.h\"\nEOF\n",
            upstream.display(),
            pinned_commit.trim()
        ),
    );

    sync_registry(&options).expect("second sync should rematerialize depofile cache demo");
    assert_eq!(
        fs::read_to_string(&installed_header).expect("read second depofile cache header"),
        "// from two\n"
    );
    let log = fs::read_to_string(sandbox.package_log_path(
        "depofile_cache_demo",
        RELEASE_NAMESPACE,
        "1.0.0",
    ))
    .expect("read depofile cache log");
    assert!(!log.contains("materialization already up to date"));
    assert!(log.contains("reuse exact git commit"));
}

#[test]
fn sync_allows_overlapping_export_paths_in_isolated_package_roots() {
    let sandbox = Sandbox::new();
    let owner = sandbox.create_git_repo(
        "upstreams/conflict_owner",
        &[("include/conflict/demo.h", "// owner\n")],
    );
    let contender = sandbox.create_git_repo(
        "upstreams/conflict_contender",
        &[("include/conflict/demo.h", "// contender\n")],
    );
    sandbox.write(
        "depofiles/local/conflict_owner/release/1.0.0/main.DepoFile",
        &format!(
            "NAME conflict_owner\nVERSION 1.0.0\nTARGET conflict_owner::conflict_owner INTERFACE include\nSOURCE GIT {} HEAD\n",
            owner.display()
        ),
    );
    sandbox.write(
        "depofiles/local/conflict_contender/release/1.0.0/main.DepoFile",
        &format!(
            "NAME conflict_contender\nVERSION 1.0.0\nTARGET conflict_contender::conflict_contender INTERFACE include\nSOURCE GIT {} HEAD\n",
            contender.display()
        ),
    );
    sandbox.write(
        "manifests/conflict_owner.cmake",
        "depos_require(conflict_owner VERSION 1.0.0)\n",
    );
    sandbox.write(
        "manifests/conflict_contender.cmake",
        "depos_require(conflict_contender VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/conflict_owner.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("owner package should materialize");

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/conflict_contender.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("isolated package roots should allow the same relative export path");

    assert_eq!(
        fs::read_to_string(
            sandbox
                .depos_root()
                .join("store")
                .join(default_variant())
                .join("conflict_owner/release/1.0.0/include/conflict/demo.h"),
        )
        .expect("read owner header"),
        "// owner\n"
    );
    assert_eq!(
        fs::read_to_string(
            sandbox
                .depos_root()
                .join("store")
                .join(default_variant())
                .join("conflict_contender/release/1.0.0/include/conflict/demo.h"),
        )
        .expect("read contender header"),
        "// contender\n"
    );
}

#[test]
fn sync_rejects_parallel_namespaces_without_alias_when_public_targets_collide() {
    let sandbox = Sandbox::new();
    let release = sandbox.create_git_repo(
        "upstreams/twin_release",
        &[("include/twin/release.h", "// release\n")],
    );
    let dev = sandbox.create_git_repo("upstreams/twin_dev", &[("include/twin/dev.h", "// dev\n")]);
    sandbox.write(
        "depofiles/local/twin/release/1.0.0/main.DepoFile",
        &format!(
            "NAME twin\nVERSION 1.0.0\nTARGET twin::core INTERFACE include\nSOURCE GIT {} HEAD\n",
            release.display()
        ),
    );
    sandbox.write(
        "depofiles/local/twin/dev-main/git.dev/main.DepoFile",
        &format!(
            "NAME twin\nVERSION git.dev\nTARGET twin::core INTERFACE include\nSOURCE GIT {} HEAD\n",
            dev.display()
        ),
    );
    sandbox.write(
        "manifests/twin_conflict.cmake",
        "depos_require(twin VERSION 1.0.0)\ndepos_require(twin NAMESPACE dev-main VERSION git.dev)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/twin_conflict.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("parallel namespaces with the same public targets should require aliasing");
    let error_text = format!("{error:#}");
    assert!(error_text.contains("public target 'twin::core'"));
    assert!(error_text.contains("twin[release]@1.0.0"));
    assert!(error_text.contains("twin[dev-main]@git.dev"));
}

#[test]
fn sync_supports_parallel_namespaces_with_manifest_aliasing() {
    let sandbox = Sandbox::new();
    let release = sandbox.create_git_repo(
        "upstreams/twin_release_alias",
        &[("include/twin/release.h", "// release alias\n")],
    );
    let dev = sandbox.create_git_repo(
        "upstreams/twin_dev_alias",
        &[("include/twin/dev.h", "// dev alias\n")],
    );
    sandbox.write(
        "depofiles/local/twin/release/1.0.0/main.DepoFile",
        &format!(
            "NAME twin\nVERSION 1.0.0\nTARGET twin::core INTERFACE include\nSOURCE GIT {} HEAD\n",
            release.display()
        ),
    );
    sandbox.write(
        "depofiles/local/twin/dev-main/git.dev/main.DepoFile",
        &format!(
            "NAME twin\nVERSION git.dev\nTARGET twin::core INTERFACE include\nSOURCE GIT {} HEAD\n",
            dev.display()
        ),
    );
    sandbox.write(
        "manifests/twin_alias.cmake",
        "depos_require(twin VERSION 1.0.0)\ndepos_require(twin NAMESPACE dev-main VERSION git.dev AS twin_dev)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/twin_alias.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("parallel namespaces should succeed when the dev namespace is aliased");

    let targets = fs::read_to_string(output.targets_file).expect("targets.cmake should exist");
    assert!(targets.contains("add_library(twin::core ALIAS _depos_twin_release_1_0_0_t0)"));
    assert!(targets.contains("add_library(twin_dev::core ALIAS _depos_twin_dev_main_git_dev_t0)"));

    assert!(sandbox
        .package_store_path("twin", RELEASE_NAMESPACE, "1.0.0", "include/twin/release.h")
        .exists());
    assert!(sandbox
        .package_store_path("twin", "dev-main", "git.dev", "include/twin/dev.h")
        .exists());

    let lock = fs::read_to_string(output.lock_file).expect("lock.cmake should exist");
    assert!(lock.contains("twin[release]@1.0.0|AUTO|EXACT|1.0.0|_"));
    assert!(lock.contains("twin[dev-main]@git.dev|AUTO|EXACT|git.dev|twin_dev"));
}

#[test]
fn sync_materializes_local_git_package_from_named_branch() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/branch_demo",
        &[("include/branch_demo/demo.h", "// default branch\n")],
    );
    let default_branch =
        run_command_capture(&upstream, ["git", "rev-parse", "--abbrev-ref", "HEAD"]);
    run_command(
        &upstream,
        ["git", "checkout", "--quiet", "-b", "feature/demo"],
    );
    fs::write(
        upstream.join("include/branch_demo/demo.h"),
        "// feature branch\n",
    )
    .expect("write branch demo header");
    run_command(&upstream, ["git", "add", "."]);
    run_command(&upstream, ["git", "commit", "--quiet", "-m", "feature"]);
    run_command(
        &upstream,
        ["git", "checkout", "--quiet", default_branch.trim()],
    );

    sandbox.write(
        "depofiles/local/branch_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME branch_demo\nVERSION 1.0.0\nSYSTEM_LIBS INHERIT\nTARGET branch_demo::branch_demo INTERFACE include\nSOURCE GIT {} feature/demo\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/branch_demo.cmake",
        "depos_require(branch_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/branch_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize branch-based git package");

    let header = fs::read_to_string(
        sandbox
            .depos_root()
            .join("store")
            .join(default_variant())
            .join("branch_demo/release/1.0.0/include/branch_demo/demo.h"),
    )
    .expect("read branch demo header");
    assert_eq!(header, "// feature branch\n");
}

#[test]
fn sync_materializes_local_git_package_from_exact_commit() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/commit_demo",
        &[("include/commit_demo/demo.h", "// first commit\n")],
    );
    let pinned_commit = run_command_capture(&upstream, ["git", "rev-parse", "HEAD"]);
    fs::write(
        upstream.join("include/commit_demo/demo.h"),
        "// second commit\n",
    )
    .expect("write commit demo header");
    run_command(&upstream, ["git", "add", "."]);
    run_command(&upstream, ["git", "commit", "--quiet", "-m", "second"]);

    sandbox.write(
        "depofiles/local/commit_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME commit_demo\nVERSION 1.0.0\nSYSTEM_LIBS INHERIT\nTARGET commit_demo::commit_demo INTERFACE include\nSOURCE GIT {} {}\n",
            upstream.display(),
            pinned_commit.trim()
        ),
    );
    sandbox.write(
        "manifests/commit_demo.cmake",
        "depos_require(commit_demo VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/commit_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize commit-pinned git package");

    let header = fs::read_to_string(
        sandbox
            .depos_root()
            .join("store")
            .join(default_variant())
            .join("commit_demo/release/1.0.0/include/commit_demo/demo.h"),
    )
    .expect("read commit demo header");
    assert_eq!(header, "// first commit\n");

    let lock = fs::read_to_string(output.lock_file).expect("lock.cmake should exist");
    assert!(lock.contains(pinned_commit.trim()));

    let statuses = collect_statuses(&StatusOptions {
        depos_root: sandbox.depos_root(),
        name: Some("commit_demo".to_string()),
        namespace: None,
        version: Some("1.0.0".to_string()),
        refresh: false,
    })
    .expect("status should succeed");
    assert_eq!(
        statuses[0].source_ref.as_deref(),
        Some(pinned_commit.trim())
    );
    assert_eq!(
        statuses[0].source_commit.as_deref(),
        Some(pinned_commit.trim())
    );
}

#[test]
fn sync_skips_up_to_date_exact_commit_local_package_without_refetching() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/exact_commit_cached",
        &[("include/exact_commit_cached/demo.h", "// cached commit\n")],
    );
    let pinned_commit = run_command_capture(&upstream, ["git", "rev-parse", "HEAD"]);
    sandbox.write(
        "depofiles/local/exact_commit_cached/release/1.0.0/main.DepoFile",
        &format!(
            "NAME exact_commit_cached\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET exact_commit_cached::exact_commit_cached INTERFACE include\nSOURCE GIT {} {}\n",
            upstream.display(),
            pinned_commit.trim()
        ),
    );
    sandbox.write(
        "manifests/exact_commit_cached.cmake",
        "depos_require(exact_commit_cached VERSION 1.0.0)\n",
    );

    let options = SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/exact_commit_cached.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    };
    sync_registry(&options).expect("first sync should materialize exact-commit package");
    sync_registry(&options).expect("second sync should reuse exact-commit package");

    let log = fs::read_to_string(sandbox.package_log_path(
        "exact_commit_cached",
        RELEASE_NAMESPACE,
        "1.0.0",
    ))
    .expect("read cached exact-commit log");
    assert!(log.contains("reuse exact git commit"));
    assert!(log.contains("materialization already up to date"));
    assert!(!log.contains("run git fetch"));
    assert!(!log.contains("run git checkout"));
}

#[test]
fn sync_materializes_local_url_package_into_fresh_root() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_tar_archive(
        "archives/url_demo",
        &[("include/url_demo/demo.h", "// from archive\n")],
    );
    let digest = format!(
        "{:x}",
        Sha256::digest(fs::read(&archive).expect("read archive"))
    );
    sandbox.write(
        "depofiles/local/url_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME url_demo\nVERSION 1.0.0\nSYSTEM_LIBS INHERIT\nTARGET url_demo::url_demo INTERFACE include\nSOURCE URL file://{}\nSHA256 {}\n",
            archive.display(),
            digest
        ),
    );
    sandbox.write(
        "manifests/url_demo.cmake",
        "depos_require(url_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/url_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize local url package");

    assert!(sandbox
        .package_store_path(
            "url_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/url_demo/demo.h"
        )
        .exists());
}

#[test]
fn sync_skips_up_to_date_url_package_without_redownloading() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_tar_archive(
        "archives/url_cached_demo",
        &[("include/url_cached_demo/demo.h", "// cached archive\n")],
    );
    let digest = format!(
        "{:x}",
        Sha256::digest(fs::read(&archive).expect("read archive"))
    );
    sandbox.write(
        "depofiles/local/url_cached_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME url_cached_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET url_cached_demo::url_cached_demo INTERFACE include\nSOURCE URL file://{}\nSHA256 {}\n",
            archive.display(),
            digest
        ),
    );
    sandbox.write(
        "manifests/url_cached_demo.cmake",
        "depos_require(url_cached_demo VERSION 1.0.0)\n",
    );

    let options = SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/url_cached_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    };
    sync_registry(&options).expect("first sync should materialize cached url package");
    sync_registry(&options).expect("second sync should reuse cached url package");

    let log =
        fs::read_to_string(sandbox.package_log_path("url_cached_demo", RELEASE_NAMESPACE, "1.0.0"))
            .expect("read cached url log");
    assert!(log.contains("reuse cached url archive"));
    assert!(log.contains("materialization already up to date"));
    assert!(!log.contains("run curl"));
    assert!(!log.contains("extract url archive"));
}

#[test]
fn sync_rejects_local_url_archive_with_path_traversal_entries() {
    let sandbox = Sandbox::new();
    let archive = sandbox.create_malicious_tar_archive(
        "archives/url_traversal_demo",
        "evil.txt",
        "payload/../escape.txt",
        "malicious\n",
    );
    let digest = format!(
        "{:x}",
        Sha256::digest(fs::read(&archive).expect("read archive"))
    );
    sandbox.write(
        "depofiles/local/url_traversal_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME url_traversal_demo\nVERSION 1.0.0\nTARGET url_traversal_demo::url_traversal_demo INTERFACE include\nSOURCE URL file://{}\nSHA256 {}\n",
            archive.display(),
            digest
        ),
    );
    sandbox.write(
        "manifests/url_traversal_demo.cmake",
        "depos_require(url_traversal_demo VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/url_traversal_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("archive traversal entries should be rejected");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("archive"));
    assert!(rendered.contains("must not contain '..'"));
}

#[test]
fn sync_materializes_command_driven_package_into_fresh_root() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/cmd_demo",
        &[("payload/demo.h", "// built in command pipeline\n")],
    );
    sandbox.write(
        "depofiles/local/cmd_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME cmd_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET cmd_demo::cmd_demo INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/cmd_demo\" && cp \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/cmd_demo/demo.h\"\nEOF\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/cmd_demo.cmake",
        "depos_require(cmd_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/cmd_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize command-driven package");

    assert!(sandbox
        .package_store_path(
            "cmd_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/cmd_demo/demo.h"
        )
        .exists());
}

#[test]
fn sync_materializes_manual_install_tree_package_without_shell_install() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/manual_tree_demo",
        &[
            (
                "include/manual_tree_demo/demo.h",
                "// manual tree install\n",
            ),
            ("metadata/LICENSE", "manual tree license\n"),
        ],
    );
    sandbox.write(
        "depofiles/local/manual_tree_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME manual_tree_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nSTAGE_TREE SOURCE include/manual_tree_demo\nSTAGE_FILE SOURCE metadata/LICENSE share/licenses/manual_tree_demo/LICENSE\nTARGET manual_tree_demo::manual_tree_demo INTERFACE include\nARTIFACT share/licenses/manual_tree_demo/LICENSE\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/manual_tree_demo.cmake",
        "depos_require(manual_tree_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/manual_tree_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize manual install tree package");

    assert!(sandbox
        .package_store_path(
            "manual_tree_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/manual_tree_demo/demo.h"
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "manual_tree_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "share/licenses/manual_tree_demo/LICENSE"
        )
        .exists());
}

#[test]
fn sync_materializes_manual_target_directly_from_source_tree() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/manual_source_demo",
        &[(
            "include/manual_source_demo/demo.h",
            "// manual source export\n",
        )],
    );
    sandbox.write(
        "depofiles/local/manual_source_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME manual_source_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nTARGET manual_source_demo::manual_source_demo INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/manual_source_demo.cmake",
        "depos_require(manual_source_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/manual_source_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize direct source manual package");

    assert!(sandbox
        .package_store_path(
            "manual_source_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/manual_source_demo/demo.h"
        )
        .exists());
}

#[test]
fn sync_materializes_manual_target_directly_from_build_tree() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/manual_build_demo",
        &[("README.md", "manual build demo\n")],
    );
    sandbox.write(
        "depofiles/local/manual_build_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME manual_build_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD_SH <<'EOF'\ninstall -d \"${{DEPO_BUILD_DIR}}/include/manual_build_demo\" \"${{DEPO_BUILD_DIR}}/lib\"\nprintf '%s\\n' '// manual build export' > \"${{DEPO_BUILD_DIR}}/include/manual_build_demo/demo.h\"\nprintf '%s\\n' 'archive' > \"${{DEPO_BUILD_DIR}}/lib/libmanual_build_demo.a\"\nEOF\nTARGET manual_build_demo::manual_build_demo STATIC lib/libmanual_build_demo.a INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/manual_build_demo.cmake",
        "depos_require(manual_build_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/manual_build_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize direct build manual package");

    assert!(sandbox
        .package_store_path(
            "manual_build_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/manual_build_demo/demo.h"
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "manual_build_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "lib/libmanual_build_demo.a"
        )
        .exists());
}

#[test]
fn sync_rejects_manual_target_when_source_and_build_outputs_are_ambiguous() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/manual_ambiguous_demo",
        &[("include/manual_ambiguous_demo/demo.h", "// source copy\n")],
    );
    sandbox.write(
        "depofiles/local/manual_ambiguous_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME manual_ambiguous_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_BUILD_SH <<'EOF'\ninstall -d \"${{DEPO_BUILD_DIR}}/include/manual_ambiguous_demo\"\nprintf '%s\\n' '// build copy' > \"${{DEPO_BUILD_DIR}}/include/manual_ambiguous_demo/demo.h\"\nEOF\nTARGET manual_ambiguous_demo::manual_ambiguous_demo INTERFACE include\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/manual_ambiguous_demo.cmake",
        "depos_require(manual_ambiguous_demo VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/manual_ambiguous_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("sync should reject ambiguous manual direct exports");
    assert!(format!("{error:#}").contains("exists in multiple candidate outputs"));
}

#[test]
fn sync_rejects_unset_variable_in_shell_hook_by_default() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/unset_var_demo",
        &[("payload/demo.h", "// unset variable demo\n")],
    );
    sandbox.write(
        "depofiles/local/unset_var_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME unset_var_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET unset_var_demo::unset_var_demo INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -d \"${{DEPO_PREFIX}}/include/unset_var_demo\"\nprintf '%s\\n' \"$UNSET_DEPO_VARIABLE\" > \"${{DEPO_PREFIX}}/include/unset_var_demo/demo.h\"\nEOF\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/unset_var_demo.cmake",
        "depos_require(unset_var_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/unset_var_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("shell hooks should fail on unset variables by default");
}

#[test]
fn sync_materializes_dependencies_before_dependents() {
    let sandbox = Sandbox::new();
    let provider = sandbox.create_git_repo(
        "upstreams/zzz_provider",
        &[("payload/provider.h", "// provider\n")],
    );
    let consumer = sandbox.create_git_repo(
        "upstreams/aaa_consumer",
        &[("payload/consumer.h", "// consumer\n")],
    );
    sandbox.write(
        "depofiles/local/zzz_provider/release/1.0.0/main.DepoFile",
        &format!(
            "NAME zzz_provider\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET zzz_provider::zzz_provider INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/zzz_provider\" && cp \"${{DEPO_SOURCE_DIR}}/payload/provider.h\" \"${{DEPO_PREFIX}}/include/zzz_provider/provider.h\"\nEOF\n",
            provider.display()
        ),
    );
    sandbox.write(
        "depofiles/local/aaa_consumer/release/1.0.0/main.DepoFile",
        &format!(
            "NAME aaa_consumer\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nDEPENDS zzz_provider VERSION 1.0.0\nTARGET aaa_consumer::aaa_consumer INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ntest -f \"$DEPO_DEP_ZZZ_PROVIDER_RELEASE_ROOT/include/zzz_provider/provider.h\" && mkdir -p \"${{DEPO_PREFIX}}/include/aaa_consumer\" && cp \"${{DEPO_SOURCE_DIR}}/payload/consumer.h\" \"${{DEPO_PREFIX}}/include/aaa_consumer/consumer.h\"\nEOF\n",
            consumer.display()
        ),
    );
    sandbox.write(
        "manifests/dependency_order_demo.cmake",
        "depos_require(aaa_consumer VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/dependency_order_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize dependency packages before dependents");

    assert!(sandbox
        .package_store_path(
            "zzz_provider",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/zzz_provider/provider.h"
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "aaa_consumer",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/aaa_consumer/consumer.h"
        )
        .exists());
}

#[test]
fn sync_materializes_scratch_package_with_toolchain_inputs_and_isolation() {
    let sandbox = Sandbox::new();
    let provider = sandbox.create_git_repo(
        "upstreams/scratch_provider",
        &[("payload/provider.h", "// scratch provider\n")],
    );
    let upstream = sandbox.create_git_repo(
        "upstreams/scratch_demo",
        &[("payload/demo.h", "// built in scratch mode\n")],
    );
    sandbox.write(
        "depofiles/local/scratch_provider/release/1.0.0/main.DepoFile",
        &format!(
            "NAME scratch_provider\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nTARGET scratch_provider::scratch_provider INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/scratch_provider\" && cp \"${{DEPO_SOURCE_DIR}}/payload/provider.h\" \"${{DEPO_PREFIX}}/include/scratch_provider/provider.h\"\nEOF\n",
            provider.display()
        ),
    );
    sandbox.write(
        "depofiles/local/scratch_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME scratch_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT SCRATCH\nDEPENDS scratch_provider VERSION 1.0.0\nTARGET scratch_demo::scratch_demo INTERFACE include\nSOURCE GIT {} HEAD\n{}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ntest -f \"$DEPO_DEP_SCRATCH_PROVIDER_RELEASE_ROOT/include/scratch_provider/provider.h\" && command -v install >/dev/null 2>&1 && ! command -v ls >/dev/null 2>&1 && test ! -e /etc/os-release && install -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/scratch_demo/demo.h\"\nEOF\n",
            upstream.display(),
            scratch_toolchain_lines()
        ),
    );
    sandbox.write(
        "manifests/scratch_demo.cmake",
        "depos_require(scratch_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/scratch_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize scratch package");

    assert!(sandbox
        .package_store_path(
            "scratch_provider",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/scratch_provider/provider.h"
        )
        .exists());
    assert!(sandbox
        .package_store_path(
            "scratch_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/scratch_demo/demo.h"
        )
        .exists());
}

#[test]
fn sync_rejects_scratch_package_without_toolchain_inputs() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/scratch_missing_toolchain",
        &[("payload/demo.h", "// missing toolchain inputs\n")],
    );
    sandbox.write(
        "depofiles/local/scratch_missing_toolchain/release/1.0.0/main.DepoFile",
        &format!(
            "NAME scratch_missing_toolchain\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT SCRATCH\nTARGET scratch_missing_toolchain::scratch_missing_toolchain INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\n:\nEOF\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/scratch_missing_toolchain.cmake",
        "depos_require(scratch_missing_toolchain VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/scratch_missing_toolchain.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("scratch builds without TOOLCHAIN_INPUT should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text
            .contains("uses BUILD_ROOT SCRATCH but does not declare any TOOLCHAIN_INPUT entries"),
        "{error:#}"
    );
}

#[test]
fn sync_rejects_scratch_package_with_rootfs_toolchain() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/scratch_rootfs_toolchain",
        &[("payload/demo.h", "// scratch rootfs toolchain\n")],
    );
    sandbox.write(
        "depofiles/local/scratch_rootfs_toolchain/release/1.0.0/main.DepoFile",
        &format!(
            "NAME scratch_rootfs_toolchain\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT SCRATCH\nTOOLCHAIN ROOTFS\nTARGET scratch_rootfs_toolchain::scratch_rootfs_toolchain INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\n:\nEOF\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/scratch_rootfs_toolchain.cmake",
        "depos_require(scratch_rootfs_toolchain VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/scratch_rootfs_toolchain.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("scratch builds with TOOLCHAIN ROOTFS should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains("uses BUILD_ROOT SCRATCH with TOOLCHAIN ROOTFS"),
        "{error:#}"
    );
}

#[test]
fn sync_rejects_scratch_package_with_relative_toolchain_input() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/scratch_relative_toolchain",
        &[("payload/demo.h", "// relative toolchain input\n")],
    );
    sandbox.write(
        "depofiles/local/scratch_relative_toolchain/release/1.0.0/main.DepoFile",
        &format!(
            "NAME scratch_relative_toolchain\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT SCRATCH\nTOOLCHAIN_INPUT relative/tool\nTARGET scratch_relative_toolchain::scratch_relative_toolchain INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\n:\nEOF\n",
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/scratch_relative_toolchain.cmake",
        "depos_require(scratch_relative_toolchain VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/scratch_relative_toolchain.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("relative TOOLCHAIN_INPUT should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains("must be an absolute host path"),
        "{error:#}"
    );
}

#[test]
fn sync_rejects_scratch_package_with_missing_toolchain_input() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/scratch_missing_input_path",
        &[("payload/demo.h", "// missing toolchain input path\n")],
    );
    let missing_path = sandbox.root.path().join("missing-toolchain-path");
    sandbox.write(
        "depofiles/local/scratch_missing_input_path/release/1.0.0/main.DepoFile",
        &format!(
            "NAME scratch_missing_input_path\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT SCRATCH\nTOOLCHAIN_INPUT {}\nTARGET scratch_missing_input_path::scratch_missing_input_path INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\n:\nEOF\n",
            missing_path.display(),
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/scratch_missing_input_path.cmake",
        "depos_require(scratch_missing_input_path VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/scratch_missing_input_path.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("missing TOOLCHAIN_INPUT should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains("does not exist on the host"),
        "{error:#}"
    );
}

#[test]
fn sync_rejects_foreign_scratch_command_pipeline() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/foreign_scratch_demo",
        &[("payload/demo.h", "// unsupported foreign scratch build\n")],
    );
    sandbox.write(
        "depofiles/local/foreign_scratch_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME foreign_scratch_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT SCRATCH\nBUILD_ARCH {}\nTARGET_ARCH {}\nTARGET foreign_scratch_demo::foreign_scratch_demo INTERFACE include\nSOURCE GIT {} HEAD\n{}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/foreign_scratch_demo/demo.h\"\nEOF\n",
            foreign_arch(),
            foreign_arch(),
            upstream.display(),
            scratch_toolchain_lines()
        ),
    );
    sandbox.write(
        "manifests/foreign_scratch_demo.cmake",
        "depos_require(foreign_scratch_demo VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/foreign_scratch_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("foreign scratch builds should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text
            .contains("only BUILD_ROOT OCI + TOOLCHAIN ROOTFS with BUILD_ARCH == TARGET_ARCH"),
        "{error:#}"
    );
}

#[test]
fn sync_rejects_cross_target_scratch_command_pipeline() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/cross_target_scratch_demo",
        &[(
            "payload/demo.h",
            "// unsupported cross-target scratch build\n",
        )],
    );
    sandbox.write(
        "depofiles/local/cross_target_scratch_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME cross_target_scratch_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT SCRATCH\nBUILD_ARCH {}\nTARGET_ARCH {}\nTARGET cross_target_scratch_demo::cross_target_scratch_demo INTERFACE include\nSOURCE GIT {} HEAD\n{}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/cross_target_scratch_demo/demo.h\"\nEOF\n",
            host_arch(),
            foreign_arch(),
            upstream.display(),
            scratch_toolchain_lines()
        ),
    );
    sandbox.write(
        "manifests/cross_target_scratch_demo.cmake",
        "depos_require(cross_target_scratch_demo VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/cross_target_scratch_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("cross-target scratch builds should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains(
            "only BUILD_ROOT OCI + TOOLCHAIN ROOTFS is restored for BUILD_ARCH != TARGET_ARCH"
        ),
        "{error:#}"
    );
}

#[test]
fn sync_materializes_oci_rootfs_package_into_fresh_root() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/oci_demo",
        &[("payload/demo.h", "// built in oci mode\n")],
    );
    let image_ref = sandbox.create_local_oci_layout_with_install("oci/base");
    sandbox.write(
        "depofiles/local/oci_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME oci_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT OCI {}\nTOOLCHAIN ROOTFS\nTARGET oci_demo::oci_demo INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/oci_demo/demo.h\"\nEOF\n",
            image_ref,
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/oci_demo.cmake",
        "depos_require(oci_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/oci_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize OCI-rootfs package");

    assert!(sandbox
        .package_store_path(
            "oci_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/oci_demo/demo.h"
        )
        .exists());
    let cache_entries = fs::read_dir(sandbox.depos_root().join("oci-cache"))
        .expect("read OCI cache")
        .map(|entry| entry.expect("cache entry").path())
        .collect::<Vec<_>>();
    assert_eq!(cache_entries.len(), 1, "expected one OCI cache entry");
    assert!(cache_entries[0].join("layout/index.json").is_file());
    assert!(cache_entries[0].join("reference.txt").is_file());
}

#[test]
fn sync_materializes_oci_rootfs_package_with_toolchain_inputs() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/oci_toolchain_demo",
        &[(
            "payload/demo.h",
            "// built in oci mode via host toolchain inputs\n",
        )],
    );
    let image_ref = sandbox.create_local_oci_layout("oci/toolchain-base");
    sandbox.write(
        "depofiles/local/oci_toolchain_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME oci_toolchain_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT OCI {}\nTOOLCHAIN ROOTFS\nTARGET oci_toolchain_demo::oci_toolchain_demo INTERFACE include\nSOURCE GIT {} HEAD\n{}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/oci_toolchain_demo/demo.h\"\nEOF\n",
            image_ref,
            upstream.display(),
            scratch_toolchain_lines()
        ),
    );
    sandbox.write(
        "manifests/oci_toolchain_demo.cmake",
        "depos_require(oci_toolchain_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/oci_toolchain_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize OCI-rootfs package via TOOLCHAIN_INPUT");

    assert!(sandbox
        .package_store_path(
            "oci_toolchain_demo",
            RELEASE_NAMESPACE,
            "1.0.0",
            "include/oci_toolchain_demo/demo.h"
        )
        .exists());
}

#[test]
fn sync_materializes_foreign_oci_rootfs_package_with_toolchain_inputs() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/foreign_oci_toolchain_demo",
        &[(
            "payload/demo.h",
            "// built in foreign oci mode via host toolchain inputs\n",
        )],
    );
    let image_ref = sandbox.create_local_oci_layout("oci/foreign-toolchain-base");
    sandbox.write(
        "depofiles/local/foreign_oci_toolchain_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME foreign_oci_toolchain_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT OCI {}\nTOOLCHAIN ROOTFS\nBUILD_ARCH {}\nTARGET_ARCH {}\nTARGET foreign_oci_toolchain_demo::foreign_oci_toolchain_demo INTERFACE include\nSOURCE GIT {} HEAD\n{}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/foreign_oci_toolchain_demo/demo.h\"\nEOF\n",
            image_ref,
            foreign_arch(),
            foreign_arch(),
            upstream.display(),
            scratch_toolchain_lines()
        ),
    );
    sandbox.write(
        "manifests/foreign_oci_toolchain_demo.cmake",
        "depos_require(foreign_oci_toolchain_demo VERSION 1.0.0)\n",
    );

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/foreign_oci_toolchain_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("sync should materialize foreign OCI-rootfs package via TOOLCHAIN_INPUT");

    let target_variant = variant_for_test_arch(foreign_arch());
    let header = fs::read_to_string(
        sandbox
            .depos_root()
            .join("store")
            .join(target_variant)
            .join(
            "foreign_oci_toolchain_demo/release/1.0.0/include/foreign_oci_toolchain_demo/demo.h",
        ),
    )
    .expect("read foreign OCI toolchain demo header");
    assert_eq!(
        header,
        "// built in foreign oci mode via host toolchain inputs\n"
    );
}

#[test]
fn sync_rejects_foreign_system_command_pipeline() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/foreign_system_demo",
        &[("payload/demo.h", "// unsupported foreign system build\n")],
    );
    sandbox.write(
        "depofiles/local/foreign_system_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME foreign_system_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ARCH {}\nTARGET_ARCH {}\nTARGET foreign_system_demo::foreign_system_demo INTERFACE include\nSOURCE GIT {} HEAD\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\nmkdir -p \"${{DEPO_PREFIX}}/include/foreign_system_demo\" && cp \"${{DEPO_SOURCE_DIR}}/payload/demo.h\" \"${{DEPO_PREFIX}}/include/foreign_system_demo/demo.h\"\nEOF\n",
            foreign_arch(),
            foreign_arch(),
            upstream.display()
        ),
    );
    sandbox.write(
        "manifests/foreign_system_demo.cmake",
        "depos_require(foreign_system_demo VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/foreign_system_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("foreign system builds should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains("only BUILD_ROOT OCI + TOOLCHAIN ROOTFS"),
        "{error:#}"
    );
}

#[test]
fn sync_materializes_cross_target_oci_rootfs_package_into_target_variant() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/foreign_cross_demo",
        &[(
            &format!("payload/{}-to-{}.h", host_arch(), foreign_arch()),
            "// cross target artifact\n",
        )],
    );
    let image_ref = sandbox.create_local_oci_layout("oci/foreign-cross-base");
    sandbox.write(
        "depofiles/local/foreign_cross_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME foreign_cross_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT OCI {}\nTOOLCHAIN ROOTFS\nBUILD_ARCH {}\nTARGET_ARCH {}\nTARGET foreign_cross_demo::foreign_cross_demo INTERFACE include\nSOURCE GIT {} HEAD\n{}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/${{DEPO_BUILD_ARCH}}-to-${{DEPO_TARGET_ARCH}}.h\" \"${{DEPO_PREFIX}}/include/foreign_cross_demo/demo.h\"\nEOF\n",
            image_ref,
            host_arch(),
            foreign_arch(),
            upstream.display()
            ,
            scratch_toolchain_lines()
        ),
    );
    sandbox.write(
        "manifests/foreign_cross_demo.cmake",
        "depos_require(foreign_cross_demo VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/foreign_cross_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("cross-target OCI package should materialize");

    let target_variant = variant_for_test_arch(foreign_arch());
    assert!(
        output
            .registry_dir
            .to_string_lossy()
            .contains(&format!("/registry/{target_variant}/")),
        "{}",
        output.registry_dir.display()
    );
    let header = fs::read_to_string(
        sandbox
            .depos_root()
            .join("store")
            .join(&target_variant)
            .join("foreign_cross_demo/release/1.0.0/include/foreign_cross_demo/demo.h"),
    )
    .expect("read cross-target header");
    assert_eq!(header, "// cross target artifact\n");

    let registry_dir = registry_dir_from_manifest(
        &sandbox.depos_root(),
        &sandbox
            .depos_root()
            .join("manifests/foreign_cross_demo.cmake"),
    )
    .expect("registry dir should resolve");
    assert_eq!(registry_dir, output.registry_dir);

    let statuses = collect_statuses(&StatusOptions {
        depos_root: sandbox.depos_root(),
        name: Some("foreign_cross_demo".to_string()),
        namespace: None,
        version: Some("1.0.0".to_string()),
        refresh: true,
    })
    .expect("status should refresh");
    assert_eq!(statuses[0].state, PackageState::Green);
    assert!(statuses[0].message.contains(&target_variant));
}

#[test]
fn sync_materializes_cross_target_oci_when_build_arch_is_foreign() {
    let sandbox = Sandbox::new();
    let upstream = sandbox.create_git_repo(
        "upstreams/foreign_build_cross_demo",
        &[(
            &format!("payload/{}-to-{}.h", foreign_arch(), host_arch()),
            "// foreign build cross target artifact\n",
        )],
    );
    let image_ref = sandbox.create_local_oci_layout("oci/foreign-build-cross-base");
    sandbox.write(
        "depofiles/local/foreign_build_cross_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME foreign_build_cross_demo\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nBUILD_ROOT OCI {}\nTOOLCHAIN ROOTFS\nBUILD_ARCH {}\nTARGET_ARCH {}\nTARGET foreign_build_cross_demo::foreign_build_cross_demo INTERFACE include\nSOURCE GIT {} HEAD\n{}\nBUILD_SYSTEM MANUAL\nMANUAL_INSTALL_SH <<'EOF'\ninstall -D \"${{DEPO_SOURCE_DIR}}/payload/${{DEPO_BUILD_ARCH}}-to-${{DEPO_TARGET_ARCH}}.h\" \"${{DEPO_PREFIX}}/include/foreign_build_cross_demo/demo.h\"\nEOF\n",
            image_ref,
            foreign_arch(),
            host_arch(),
            upstream.display(),
            scratch_toolchain_lines()
        ),
    );
    sandbox.write(
        "manifests/foreign_build_cross_demo.cmake",
        "depos_require(foreign_build_cross_demo VERSION 1.0.0)\n",
    );

    let output = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/foreign_build_cross_demo.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("foreign-build cross-target OCI package should materialize");

    let target_variant = variant_for_test_arch(host_arch().as_str());
    assert!(
        output
            .registry_dir
            .to_string_lossy()
            .contains(&format!("/registry/{target_variant}/")),
        "{}",
        output.registry_dir.display()
    );
    let header = fs::read_to_string(
        sandbox
            .depos_root()
            .join("store")
            .join(&target_variant)
            .join("foreign_build_cross_demo/release/1.0.0/include/foreign_build_cross_demo/demo.h"),
    )
    .expect("read foreign-build cross-target header");
    assert_eq!(header, "// foreign build cross target artifact\n");
}

#[test]
fn sync_rejects_manifest_with_multiple_target_arches() {
    let sandbox = Sandbox::new();
    let host_upstream = sandbox.create_git_repo(
        "upstreams/host_mix_demo",
        &[("include/host_mix_demo/demo.h", "// host mix\n")],
    );
    let foreign_upstream = sandbox.create_git_repo(
        "upstreams/foreign_mix_demo",
        &[("include/foreign_mix_demo/demo.h", "// foreign mix\n")],
    );
    sandbox.write(
        "depofiles/local/host_mix_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME host_mix_demo\nVERSION 1.0.0\nTARGET host_mix_demo::host_mix_demo INTERFACE include\nSOURCE GIT {} HEAD\n",
            host_upstream.display()
        ),
    );
    sandbox.write(
        "depofiles/local/foreign_mix_demo/release/1.0.0/main.DepoFile",
        &format!(
            "NAME foreign_mix_demo\nVERSION 1.0.0\nBUILD_ARCH {}\nTARGET_ARCH {}\nTARGET foreign_mix_demo::foreign_mix_demo INTERFACE include\nSOURCE GIT {} HEAD\n",
            host_arch(),
            foreign_arch(),
            foreign_upstream.display()
        ),
    );
    sandbox.write(
        "manifests/mixed_targets.cmake",
        "depos_require(host_mix_demo VERSION 1.0.0)\ndepos_require(foreign_mix_demo VERSION 1.0.0)\n",
    );

    let error = sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox.depos_root().join("manifests/mixed_targets.cmake"),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("mixed target architectures should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains("multiple TARGET_ARCH values"),
        "{error:#}"
    );
}

#[test]
fn cmake_source_library_matrix_cascades_dependencies() {
    let sandbox = Sandbox::new();
    let base_repo = create_demo_static_library_repo(
        &sandbox,
        "upstreams/source_matrix_base_math",
        "base_math",
        "base_math",
        40,
    );
    let adder_repo = create_demo_static_library_repo(
        &sandbox,
        "upstreams/source_matrix_adder",
        "adder",
        "adder",
        2,
    );

    for source_style in ["all", "explicit"] {
        for resolution_mode in ["explicit", "bootstrap"] {
            let scenario = format!("source-{source_style}-{resolution_mode}");
            let (library_repo, _) =
                create_cascade_library_repo(&sandbox, &scenario, &base_repo, &adder_repo);
            let build_dir = sandbox.depos_root().join("cmake-builds").join(&scenario);
            let mut definitions =
                vec![("CASCADE_SOURCE_STYLE".to_string(), source_style.to_string())];
            let envs = cmake_resolution_env(
                &sandbox,
                &scenario,
                resolution_mode,
                &library_repo,
                &mut definitions,
            );

            configure_build_and_test_cmake(&library_repo, &build_dir, &definitions, &envs);

            assert!(
                library_repo.join(".depos/.state.cmake").exists(),
                "missing state for scenario {scenario}"
            );
            if resolution_mode == "bootstrap" {
                assert!(
                    library_repo.join(".depos/.tool/bin/depos").exists(),
                    "missing bootstrapped binary for scenario {scenario}"
                );
                assert!(
                    library_repo.join(".depos/.root").exists(),
                    "missing hidden local root for scenario {scenario}"
                );
            } else {
                assert!(
                    !library_repo.join(".depos/.tool/bin/depos").exists(),
                    "unexpected bootstrapped binary for explicit scenario {scenario}"
                );
            }
        }
    }
}

#[test]
fn cmake_depofile_consumer_matrix_cascades_transitive_dependencies() {
    let sandbox = Sandbox::new();
    let base_repo = create_demo_static_library_repo(
        &sandbox,
        "upstreams/consumer_matrix_base_math",
        "base_math",
        "base_math",
        40,
    );
    let adder_repo = create_demo_static_library_repo(
        &sandbox,
        "upstreams/consumer_matrix_adder",
        "adder",
        "adder",
        2,
    );

    for consumer_mode in ["package", "path"] {
        for resolution_mode in ["explicit", "bootstrap"] {
            let scenario = format!("consumer-{consumer_mode}-{resolution_mode}");
            let (library_repo, published_depofile) =
                create_cascade_library_repo(&sandbox, &scenario, &base_repo, &adder_repo);
            let consumer_root = sandbox.depos_root().join("consumers").join(&scenario);
            create_cascade_consumer_project(
                &consumer_root,
                &library_repo,
                &published_depofile,
                consumer_mode,
            );

            let build_dir = consumer_root.join("build");
            let mut definitions = vec![
                (
                    "CASCADE_CONSUMER_MODE".to_string(),
                    consumer_mode.to_string(),
                ),
                (
                    "CASCADE_LIBRARY_DEPOFILE".to_string(),
                    published_depofile.display().to_string(),
                ),
            ];
            let envs = cmake_resolution_env(
                &sandbox,
                &scenario,
                resolution_mode,
                &consumer_root,
                &mut definitions,
            );

            configure_build_and_test_cmake(&consumer_root, &build_dir, &definitions, &envs);

            assert!(
                consumer_root.join(".depos/.state.cmake").exists(),
                "missing consumer state for scenario {scenario}"
            );
            if resolution_mode == "bootstrap" {
                assert!(
                    consumer_root.join(".depos/.tool/bin/depos").exists(),
                    "missing bootstrapped binary for consumer scenario {scenario}"
                );
                assert!(
                    consumer_root.join(".depos/.root").exists(),
                    "missing hidden local root for consumer scenario {scenario}"
                );
            } else {
                assert!(
                    !consumer_root.join(".depos/.tool/bin/depos").exists(),
                    "unexpected bootstrapped binary for explicit consumer scenario {scenario}"
                );
            }
        }
    }
}

fn depofile_demo() -> String {
    "NAME demo\nVERSION 1.0.0\nSYSTEM_LIBS INHERIT\nTARGET demo::demo INTERFACE include\nSOURCE URL https://example.com/demo.tar.gz\nSHA256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n".to_string()
}

fn create_demo_static_library_repo(
    sandbox: &Sandbox,
    relative: &str,
    package_name: &str,
    function_stem: &str,
    value: i32,
) -> PathBuf {
    sandbox.create_git_repo_owned(
        relative,
        &[
            (
                "CMakeLists.txt".to_string(),
                format!(
                    "cmake_minimum_required(VERSION 3.21)\nproject({package_name} LANGUAGES CXX)\nadd_library({package_name} STATIC src/{package_name}.cpp)\ntarget_compile_features({package_name} PUBLIC cxx_std_20)\ntarget_include_directories({package_name} PUBLIC ${{CMAKE_CURRENT_SOURCE_DIR}}/include)\ninstall(TARGETS {package_name} ARCHIVE DESTINATION lib)\ninstall(DIRECTORY include/ DESTINATION include)\n"
                ),
            ),
            (
                format!("src/{package_name}.cpp"),
                format!(
                    "#include <{package_name}/{package_name}.h>\n\nint {function_stem}_value() {{\n    return {value};\n}}\n"
                ),
            ),
            (
                format!("include/{package_name}/{package_name}.h"),
                format!("#pragma once\n\nint {function_stem}_value();\n"),
            ),
        ],
    )
}

fn create_cascade_library_repo(
    sandbox: &Sandbox,
    relative: &str,
    base_repo: &Path,
    adder_repo: &Path,
) -> (PathBuf, PathBuf) {
    let library_repo = sandbox.depos_root().join("libraries").join(relative);
    let repo = sandbox.create_git_repo_owned(
        &format!("libraries/{relative}"),
        &[
            (
                ".depos.cmake".to_string(),
                fs::read_to_string("/root/depos/.depos.cmake").expect("read repo helper"),
            ),
            (
                "CMakeLists.txt".to_string(),
                "cmake_minimum_required(VERSION 3.21)\nproject(cascade_lib LANGUAGES CXX)\n\nset(_cascade_base_root \"\")\nset(_cascade_adder_root \"\")\nif (DEFINED ENV{CMAKE_PREFIX_PATH} AND NOT \"$ENV{CMAKE_PREFIX_PATH}\" STREQUAL \"\")\n  set(_cascade_prefix_roots \"$ENV{CMAKE_PREFIX_PATH}\")\n  foreach(_cascade_root IN LISTS _cascade_prefix_roots)\n    if (_cascade_base_root STREQUAL \"\" AND EXISTS \"${_cascade_root}/include/base_math/base_math.h\")\n      set(_cascade_base_root \"${_cascade_root}\")\n    endif()\n    if (_cascade_adder_root STREQUAL \"\" AND EXISTS \"${_cascade_root}/include/adder/adder.h\")\n      set(_cascade_adder_root \"${_cascade_root}\")\n    endif()\n  endforeach()\nendif()\n\nif (NOT _cascade_base_root STREQUAL \"\" AND NOT _cascade_adder_root STREQUAL \"\")\n  add_library(base_math::base_math STATIC IMPORTED GLOBAL)\n  set_target_properties(base_math::base_math PROPERTIES\n    IMPORTED_LOCATION \"${_cascade_base_root}/lib/libbase_math.a\"\n    INTERFACE_INCLUDE_DIRECTORIES \"${_cascade_base_root}/include\"\n  )\n  add_library(adder::adder STATIC IMPORTED GLOBAL)\n  set_target_properties(adder::adder PROPERTIES\n    IMPORTED_LOCATION \"${_cascade_adder_root}/lib/libadder.a\"\n    INTERFACE_INCLUDE_DIRECTORIES \"${_cascade_adder_root}/include\"\n  )\nelse()\n  include(\"${CMAKE_CURRENT_LIST_DIR}/.depos.cmake\")\n  set(CASCADE_SOURCE_STYLE \"all\" CACHE STRING \"all or explicit\")\n  if (CASCADE_SOURCE_STYLE STREQUAL \"all\")\n    depos_depend_all()\n    set(_cascade_link_mode ALL)\n  elseif (CASCADE_SOURCE_STYLE STREQUAL \"explicit\")\n    depos_depend(base_math VERSION 1.0.0)\n    depos_depend(adder VERSION 1.0.0)\n    set(_cascade_link_mode EXPLICIT)\n  else()\n    message(FATAL_ERROR \"Unsupported CASCADE_SOURCE_STYLE=${CASCADE_SOURCE_STYLE}\")\n  endif()\nendif()\n\nadd_library(cascade_lib STATIC src/cascade_lib.cpp)\ntarget_compile_features(cascade_lib PUBLIC cxx_std_20)\ntarget_include_directories(cascade_lib PUBLIC \"${CMAKE_CURRENT_SOURCE_DIR}/include\")\nif (DEFINED _cascade_link_mode)\n  if (_cascade_link_mode STREQUAL \"ALL\")\n    depos_link_all(cascade_lib)\n  else()\n    depos_link(cascade_lib base_math adder)\n  endif()\nelse()\n  target_link_libraries(cascade_lib PUBLIC base_math::base_math adder::adder)\nendif()\ninstall(TARGETS cascade_lib ARCHIVE DESTINATION lib)\ninstall(DIRECTORY include/ DESTINATION include)\n\nif (PROJECT_IS_TOP_LEVEL)\n  add_executable(cascade_lib_smoke app/main.cpp)\n  target_compile_features(cascade_lib_smoke PRIVATE cxx_std_20)\n  target_link_libraries(cascade_lib_smoke PRIVATE cascade_lib)\n  enable_testing()\n  add_test(NAME cascade-lib-smoke COMMAND cascade_lib_smoke)\nendif()\n"
                    .to_string(),
            ),
            (
                "src/cascade_lib.cpp".to_string(),
                "#include <adder/adder.h>\n#include <base_math/base_math.h>\n#include <cascade_lib/cascade_lib.h>\n\nint cascade_lib_value() {\n    return base_math_value() + adder_value();\n}\n"
                    .to_string(),
            ),
            (
                "include/cascade_lib/cascade_lib.h".to_string(),
                "#pragma once\n\nint cascade_lib_value();\n".to_string(),
            ),
            (
                "app/main.cpp".to_string(),
                "#include <cascade_lib/cascade_lib.h>\n\nint main() {\n    return cascade_lib_value() == 42 ? 0 : 1;\n}\n"
                    .to_string(),
            ),
            (
                "depofiles/base_math/release/1.0.0/main.DepoFile".to_string(),
                format!(
                    "NAME base_math\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nBUILD_SYSTEM CMAKE\nTARGET base_math::base_math STATIC lib/libbase_math.a INTERFACE include\n",
                    base_repo.display()
                ),
            ),
            (
                "depofiles/adder/release/1.0.0/main.DepoFile".to_string(),
                format!(
                    "NAME adder\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nBUILD_SYSTEM CMAKE\nTARGET adder::adder STATIC lib/libadder.a INTERFACE include\n",
                    adder_repo.display()
                ),
            ),
            (
                "published/cascade_lib.DepoFile".to_string(),
                format!(
                    "NAME cascade_lib\nVERSION 1.0.0\nSYSTEM_LIBS NEVER\nSOURCE GIT {} HEAD\nBUILD_SYSTEM CMAKE\nDEPENDS base_math VERSION 1.0.0\nDEPENDS adder VERSION 1.0.0\nTARGET cascade_lib::cascade_lib STATIC lib/libcascade_lib.a INTERFACE include\n",
                    library_repo.display()
                ),
            ),
        ],
    );
    let published_depofile = repo.join("published/cascade_lib.DepoFile");
    (repo, published_depofile)
}

fn create_cascade_consumer_project(
    consumer_root: &Path,
    library_repo: &Path,
    published_depofile: &Path,
    consumer_mode: &str,
) {
    fs::create_dir_all(consumer_root).expect("create consumer root");
    fs::copy(
        "/root/depos/.depos.cmake",
        consumer_root.join(".depos.cmake"),
    )
    .expect("copy helper into consumer root");
    fs::write(
        consumer_root.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.21)\nproject(cascade_consumer LANGUAGES CXX)\ninclude(\"${CMAKE_CURRENT_LIST_DIR}/.depos.cmake\")\nset(CASCADE_CONSUMER_MODE \"package\" CACHE STRING \"package or path\")\nset(CASCADE_LIBRARY_DEPOFILE \"\" CACHE FILEPATH \"Published cascade library DepoFile\")\nif (CASCADE_CONSUMER_MODE STREQUAL \"package\")\n  depos_depend(cascade_lib VERSION 1.0.0)\nelseif (CASCADE_CONSUMER_MODE STREQUAL \"path\")\n  depos_depend(\"${CASCADE_LIBRARY_DEPOFILE}\")\nelse()\n  message(FATAL_ERROR \"Unsupported CASCADE_CONSUMER_MODE=${CASCADE_CONSUMER_MODE}\")\nendif()\nadd_executable(cascade_consumer main.cpp)\ntarget_compile_features(cascade_consumer PRIVATE cxx_std_20)\ndepos_link(cascade_consumer cascade_lib)\nenable_testing()\nadd_test(NAME cascade-consumer COMMAND cascade_consumer)\n",
    )
    .expect("write consumer CMakeLists");
    fs::write(
        consumer_root.join("main.cpp"),
        "#include <cascade_lib/cascade_lib.h>\n\nint main() {\n    return cascade_lib_value() == 42 ? 0 : 1;\n}\n",
    )
    .expect("write consumer main");

    if consumer_mode == "package" {
        copy_tree(
            &library_repo.join("depofiles"),
            &consumer_root.join("depofiles"),
        );
        let consumer_library_depofile =
            consumer_root.join("depofiles/cascade_lib/release/1.0.0/main.DepoFile");
        if let Some(parent) = consumer_library_depofile.parent() {
            fs::create_dir_all(parent).expect("create consumer library depofile parent");
        }
        fs::copy(published_depofile, consumer_library_depofile)
            .expect("copy published depofile into consumer depofiles");
    }
}

#[test]
fn cmake_depend_functions_emit_status_updates() {
    let sandbox = Sandbox::new();
    let smoke_source = Path::new("/root/depos/tests/smoke");
    let depos_binary = PathBuf::from(env!("CARGO_BIN_EXE_depos"));

    let explicit_build = sandbox.depos_root().join("cmake-status").join("explicit");
    let explicit_output = configure_cmake_capture_output(
        smoke_source,
        &explicit_build,
        &[
            (
                "DEPOS_EXECUTABLE".to_string(),
                depos_binary.display().to_string(),
            ),
            (
                "DEPOS_ROOT".to_string(),
                explicit_build.join("depos-root").display().to_string(),
            ),
            ("DEPOS_SMOKE_STYLE".to_string(), "explicit".to_string()),
        ],
        &[],
    );
    assert!(explicit_output.contains("depos: requesting bitsery VERSION 5.2.3"));
    assert!(explicit_output.contains("depos: requesting zlib VERSION 1.3.2"));
    assert!(explicit_output.contains("depos: using system depos at"));
    assert_eq!(explicit_output.matches("depos: syncing ").count(), 1);
    assert!(explicit_output.contains("depos: syncing 3 dependency request(s) with system depos"));
    assert!(explicit_output.contains("depos: loaded registry targets from"));

    let files_build = sandbox.depos_root().join("cmake-status").join("files");
    let files_output = configure_cmake_capture_output(
        smoke_source,
        &files_build,
        &[
            (
                "DEPOS_EXECUTABLE".to_string(),
                depos_binary.display().to_string(),
            ),
            (
                "DEPOS_ROOT".to_string(),
                files_build.join("depos-root").display().to_string(),
            ),
            ("DEPOS_SMOKE_STYLE".to_string(), "files".to_string()),
        ],
        &[],
    );
    assert!(files_output.contains("depos: requesting bitsery VERSION 5.2.3"));
    assert!(files_output.contains("depos: requesting zlib VERSION 1.3.2"));
    assert_eq!(files_output.matches("depos: syncing ").count(), 1);
    assert!(files_output.contains("depos: syncing 3 dependency request(s) with system depos"));
    assert!(files_output.contains("depos: loaded registry targets from"));

    let all_build = sandbox.depos_root().join("cmake-status").join("all");
    let all_output = configure_cmake_capture_output(
        smoke_source,
        &all_build,
        &[
            (
                "DEPOS_EXECUTABLE".to_string(),
                depos_binary.display().to_string(),
            ),
            (
                "DEPOS_ROOT".to_string(),
                all_build.join("depos-root").display().to_string(),
            ),
            ("DEPOS_SMOKE_STYLE".to_string(), "all".to_string()),
        ],
        &[],
    );
    assert!(all_output.contains("depos: requesting all 3 DepoFile(s) under"));
    assert!(
        all_output.contains("depos: registering 3 local DepoFile(s) under namespace depos_smoke")
    );
    assert_eq!(all_output.matches("depos: syncing ").count(), 1);
    assert!(all_output.contains("depos: syncing 3 dependency request(s) with system depos"));
    assert!(all_output.contains("depos: loaded registry targets from"));
}

#[test]
fn cmake_bootstrap_reads_repo_local_project_defaults() {
    let sandbox = Sandbox::new();
    let project_root = sandbox.depos_root().join("cmake-project-defaults");
    fs::create_dir_all(&project_root).expect("create project root");
    fs::copy(
        "/root/depos/.depos.cmake",
        project_root.join(".depos.cmake"),
    )
    .expect("copy helper into project root");
    fs::copy(
        "/root/depos/tests/smoke/main.cpp",
        project_root.join("main.cpp"),
    )
    .expect("copy smoke source");
    fs::write(
        project_root.join("depos.project.cmake"),
        "set(DEPOS_BOOTSTRAP_VERSION \"9.9.9\" CACHE STRING \"Pinned depos version used by test\" FORCE)\n",
    )
    .expect("write project defaults");
    copy_tree(
        Path::new("/root/depos/tests/smoke/fixtures/local"),
        &project_root.join("depofiles"),
    );
    fs::write(
        project_root.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.21)\nproject(project_defaults_demo LANGUAGES CXX)\ninclude(\"${CMAKE_CURRENT_LIST_DIR}/.depos.cmake\")\ndepos_depend_all()\nadd_executable(project-defaults-smoke main.cpp)\ntarget_compile_features(project-defaults-smoke PRIVATE cxx_std_20)\ndepos_link_all(project-defaults-smoke)\nenable_testing()\nadd_test(NAME project-defaults-smoke COMMAND project-defaults-smoke)\n",
    )
    .expect("write project CMakeLists");

    let build_dir = sandbox.depos_root().join("cmake-project-defaults-build");
    let fake_cargo_dir =
        create_fake_cargo_dir_with_version(&sandbox, "fake-cargo/project-defaults", "9.9.9");
    let output = configure_cmake_capture_output(
        &project_root,
        &build_dir,
        &[(
            "DEPOS_ALLOW_SYSTEM_EXECUTABLE".to_string(),
            "OFF".to_string(),
        )],
        &[(
            "PATH".to_string(),
            format!(
                "{}:{}",
                fake_cargo_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )],
    );

    assert!(output.contains("depos: bootstrapping depos 9.9.9 locally with cargo install"));
    let state = fs::read_to_string(project_root.join(".depos/.state.cmake"))
        .expect("read bootstrapped project state");
    assert!(state.contains("DEPOS_STATE_VERSION [==[9.9.9]==]"));
}

fn cmake_resolution_env(
    sandbox: &Sandbox,
    scenario: &str,
    resolution_mode: &str,
    project_root: &Path,
    definitions: &mut Vec<(String, String)>,
) -> Vec<(String, String)> {
    match resolution_mode {
        "explicit" => {
            definitions.push((
                "DEPOS_EXECUTABLE".to_string(),
                env!("CARGO_BIN_EXE_depos").to_string(),
            ));
            definitions.push((
                "DEPOS_ROOT".to_string(),
                project_root
                    .join(".explicit-depos-root")
                    .display()
                    .to_string(),
            ));
            Vec::new()
        }
        "bootstrap" => {
            definitions.push((
                "DEPOS_ALLOW_SYSTEM_EXECUTABLE".to_string(),
                "OFF".to_string(),
            ));
            let fake_cargo_dir = create_fake_cargo_dir(sandbox, &format!("fake-cargo/{scenario}"));
            vec![(
                "PATH".to_string(),
                format!(
                    "{}:{}",
                    fake_cargo_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )]
        }
        other => panic!("unsupported resolution mode {other}"),
    }
}

fn configure_build_and_test_cmake(
    source_dir: &Path,
    build_dir: &Path,
    definitions: &[(String, String)],
    envs: &[(String, String)],
) {
    configure_cmake_capture_output(source_dir, build_dir, definitions, envs);

    run_command_env_vec(
        source_dir,
        &[
            "cmake".to_string(),
            "--build".to_string(),
            build_dir.display().to_string(),
            "--parallel".to_string(),
            nproc_string(),
        ],
        envs,
    );
    run_command_env_vec(
        source_dir,
        &[
            "ctest".to_string(),
            "--test-dir".to_string(),
            build_dir.display().to_string(),
            "--output-on-failure".to_string(),
        ],
        envs,
    );
}

fn configure_cmake_capture_output(
    source_dir: &Path,
    build_dir: &Path,
    definitions: &[(String, String)],
    envs: &[(String, String)],
) -> String {
    let mut configure = vec![
        "cmake".to_string(),
        "--fresh".to_string(),
        "-S".to_string(),
        source_dir.display().to_string(),
        "-B".to_string(),
        build_dir.display().to_string(),
    ];
    for (key, value) in definitions {
        configure.push(format!("-D{key}={value}"));
    }
    run_command_capture_env_vec(source_dir, &configure, envs)
}

fn create_fake_cargo_dir(sandbox: &Sandbox, relative: &str) -> PathBuf {
    create_fake_cargo_dir_with_version(sandbox, relative, "0.4.0")
}

fn create_fake_cargo_dir_with_version(sandbox: &Sandbox, relative: &str, version: &str) -> PathBuf {
    let dir = sandbox.depos_root().join(relative);
    fs::create_dir_all(&dir).expect("create fake cargo dir");
    let cargo = dir.join("cargo");
    fs::write(
        &cargo,
        format!(
            "#!/usr/bin/env bash\nset -euo pipefail\n\nif [[ \"${{1:-}}\" != \"install\" ]]; then\n  echo \"unexpected cargo invocation: $*\" >&2\n  exit 1\nfi\n\nroot=\"\"\nversion=\"\"\ncrate=\"\"\nwhile (($#)); do\n  case \"$1\" in\n    install|--locked)\n      shift\n      ;;\n    --root)\n      root=\"$2\"\n      shift 2\n      ;;\n    --version)\n      version=\"$2\"\n      shift 2\n      ;;\n    -j)\n      shift 2\n      ;;\n    depos)\n      crate=\"$1\"\n      shift\n      ;;\n    *)\n      echo \"unexpected cargo argument: $1\" >&2\n      exit 1\n      ;;\n  esac\ndone\n\ntest -n \"$root\"\ntest \"$version\" = \"{}\"\ntest \"$crate\" = \"depos\"\nmkdir -p \"$root/bin\"\ncp '{}' \"$root/bin/depos\"\n",
            version,
            env!("CARGO_BIN_EXE_depos")
        ),
    )
    .expect("write fake cargo");
    let mut perms = fs::metadata(&cargo).expect("stat fake cargo").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&cargo, perms).expect("chmod fake cargo");
    dir
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create copy_tree destination");
    let entries = fs::read_dir(source)
        .unwrap_or_else(|error| panic!("read_dir {} failed: {error}", source.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| panic!("read_dir entry failed: {error}"));
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_tree(&source_path, &destination_path);
        } else {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent).expect("create copy_tree parent");
            }
            fs::copy(&source_path, &destination_path).unwrap_or_else(|error| {
                panic!(
                    "copy {} -> {} failed: {error}",
                    source_path.display(),
                    destination_path.display()
                )
            });
        }
    }
}

fn nproc_string() -> String {
    let output = Command::new(resolve_tool("nproc"))
        .output()
        .expect("spawn nproc");
    assert!(output.status.success(), "nproc failed");
    String::from_utf8(output.stdout)
        .expect("utf-8 nproc output")
        .trim()
        .to_string()
}

fn scratch_toolchain_lines() -> String {
    let mut lines = Vec::new();
    for path in [
        "/bin/sh",
        "/usr/bin/sh",
        "/usr/bin/install",
        "/usr/lib",
        "/lib",
        "/lib64",
    ] {
        if Path::new(path).exists() {
            lines.push(format!("TOOLCHAIN_INPUT {}", path));
        }
    }
    lines.join("\n")
}

fn foreign_arch() -> &'static str {
    match host_arch().as_str() {
        "x86_64" => "aarch64",
        "aarch64" => "x86_64",
        "riscv64" => "x86_64",
        other => panic!("unsupported host arch {other}"),
    }
}

fn variant_for_test_arch(arch: &str) -> String {
    format!("{arch}-{arch}_v1")
}

fn collect_named_files(root: &Path, file_name: &str, output: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("read_dir {} failed: {error}", root.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| panic!("read_dir entry failed: {error}"));
        let path = entry.path();
        if path.is_dir() {
            collect_named_files(&path, file_name, output);
        } else if path.file_name().and_then(|value| value.to_str()) == Some(file_name) {
            output.push(path);
        }
    }
}

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(root.path().join("store").join(default_variant()))
            .expect("create store");
        let sandbox = Self { root };
        sandbox.write("depofiles/.keep", "");
        sandbox.write(".run/.keep", "");
        sandbox
    }

    fn depos_root(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    fn package_store_root(&self, name: &str, namespace: &str, version: &str) -> PathBuf {
        self.package_store_root_for_variant(&default_variant(), name, namespace, version)
    }

    fn package_store_root_for_variant(
        &self,
        variant: &str,
        name: &str,
        namespace: &str,
        version: &str,
    ) -> PathBuf {
        self.depos_root()
            .join("store")
            .join(variant)
            .join(name)
            .join(namespace)
            .join(version)
    }

    fn package_store_path(
        &self,
        name: &str,
        namespace: &str,
        version: &str,
        relative: &str,
    ) -> PathBuf {
        self.package_store_root(name, namespace, version)
            .join(relative)
    }

    fn package_log_path(&self, name: &str, namespace: &str, version: &str) -> PathBuf {
        self.depos_root()
            .join(".run")
            .join("logs")
            .join(name)
            .join(namespace)
            .join(format!("{version}.log"))
    }

    fn package_store_path_for_variant(
        &self,
        variant: &str,
        name: &str,
        namespace: &str,
        version: &str,
        relative: &str,
    ) -> PathBuf {
        self.package_store_root_for_variant(variant, name, namespace, version)
            .join(relative)
    }

    fn write_package_store(
        &self,
        name: &str,
        namespace: &str,
        version: &str,
        relative: &str,
        contents: &str,
    ) {
        let path = self.package_store_path(name, namespace, version, relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create package store parent");
        }
        fs::write(path, contents).expect("write package store file");
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
    }

    fn write_store(&self, relative: &str, contents: &str) {
        let path = self
            .depos_root()
            .join("store")
            .join(default_variant())
            .join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create store parent");
        }
        fs::write(path, contents).expect("write store file");
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

    fn create_git_repo_owned(&self, relative: &str, files: &[(String, String)]) -> PathBuf {
        let repo = self.root.path().join(relative);
        fs::create_dir_all(&repo).expect("create owned repo");
        for (path, contents) in files {
            let file_path = repo.join(path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).expect("create owned repo parent");
            }
            fs::write(file_path, contents).expect("write owned repo file");
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

    fn create_tar_archive(&self, relative: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = self.root.path().join(relative);
        let source = root.join("payload");
        fs::create_dir_all(&source).expect("create archive source");
        for (path, contents) in files {
            let file_path = source.join(path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).expect("create archive parent");
            }
            fs::write(file_path, contents).expect("write archive file");
        }
        let archive = root.join("payload.tar");
        if let Some(parent) = archive.parent() {
            fs::create_dir_all(parent).expect("create archive root");
        }
        run_command(
            self.root.path().to_path_buf().as_ref(),
            [
                "tar",
                "-cf",
                archive.to_str().expect("utf-8 archive path"),
                "-C",
                root.to_str().expect("utf-8 root path"),
                "payload",
            ],
        );
        archive
    }

    fn create_malicious_tar_archive(
        &self,
        relative: &str,
        source_name: &str,
        transformed_name: &str,
        contents: &str,
    ) -> PathBuf {
        let root = self.root.path().join(relative);
        let payload = root.join("payload");
        fs::create_dir_all(&payload).expect("create malicious archive source");
        let source_path = payload.join(source_name);
        if let Some(parent) = source_path.parent() {
            fs::create_dir_all(parent).expect("create malicious archive parent");
        }
        fs::write(&source_path, contents).expect("write malicious archive file");
        let archive = root.join("payload.tar");
        if let Some(parent) = archive.parent() {
            fs::create_dir_all(parent).expect("create malicious archive root");
        }
        run_command_vec(
            self.root.path(),
            &[
                "tar".to_string(),
                "-cf".to_string(),
                archive.display().to_string(),
                "-C".to_string(),
                root.display().to_string(),
                "--transform".to_string(),
                format!("s#^payload/{}#{}#", source_name, transformed_name),
                format!("payload/{source_name}"),
            ],
        );
        archive
    }

    fn create_local_oci_layout_with_install(&self, relative: &str) -> String {
        let root = self.root.path().join(relative);
        let layout = root.join("layout");
        let bundle = root.join("bundle");
        let image_ref = self.create_local_oci_layout(relative);
        for shell_path in ["/bin/sh", "/usr/bin/sh"] {
            let host_shell = Path::new(shell_path);
            if host_shell.exists() {
                copy_binary_with_ldd_dependencies(host_shell, &bundle.join("rootfs"));
                break;
            }
        }
        copy_binary_with_ldd_dependencies(Path::new("/usr/bin/install"), &bundle.join("rootfs"));
        run_command_vec(
            self.root.path(),
            &[
                "umoci".to_string(),
                "repack".to_string(),
                "--image".to_string(),
                format!("{}:base", layout.display()),
                bundle.display().to_string(),
            ],
        );
        image_ref
    }

    fn create_local_oci_layout(&self, relative: &str) -> String {
        let root = self.root.path().join(relative);
        let layout = root.join("layout");
        let bundle = root.join("bundle");
        fs::create_dir_all(&root).expect("create OCI root");
        run_command_vec(
            self.root.path(),
            &[
                "umoci".to_string(),
                "init".to_string(),
                "--layout".to_string(),
                layout.display().to_string(),
            ],
        );
        run_command_vec(
            self.root.path(),
            &[
                "umoci".to_string(),
                "new".to_string(),
                "--image".to_string(),
                format!("{}:base", layout.display()),
            ],
        );
        run_command_vec(
            self.root.path(),
            &[
                "umoci".to_string(),
                "unpack".to_string(),
                "--rootless".to_string(),
                "--image".to_string(),
                format!("{}:base", layout.display()),
                bundle.display().to_string(),
            ],
        );
        format!("oci:{}:base", layout.display())
    }
}

fn run_command<const N: usize>(current_dir: &Path, argv: [&str; N]) {
    let status = Command::new(resolve_tool(argv[0]))
        .args(&argv[1..])
        .current_dir(current_dir)
        .status()
        .expect("spawn command");
    assert!(status.success(), "command failed: {:?}", argv);
}

fn run_command_vec(current_dir: &Path, argv: &[String]) {
    let status = Command::new(resolve_tool(argv.first().expect("command name")))
        .args(&argv[1..])
        .current_dir(current_dir)
        .status()
        .expect("spawn command");
    assert!(status.success(), "command failed: {:?}", argv);
}

fn run_command_env_vec(current_dir: &Path, argv: &[String], envs: &[(String, String)]) {
    let mut command = Command::new(resolve_tool(argv.first().expect("command name")));
    command.args(&argv[1..]).current_dir(current_dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    let status = command.status().expect("spawn command");
    assert!(
        status.success(),
        "command failed: {:?} env={:?}",
        argv,
        envs
    );
}

fn run_command_capture_env_vec(
    current_dir: &Path,
    argv: &[String],
    envs: &[(String, String)],
) -> String {
    let mut command = Command::new(resolve_tool(argv.first().expect("command name")));
    command.args(&argv[1..]).current_dir(current_dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("spawn command");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "command failed: {:?} env={:?}\nstdout:\n{}\nstderr:\n{}",
        argv,
        envs,
        stdout,
        stderr
    );
    format!("{stdout}{stderr}")
}

fn run_command_capture<const N: usize>(current_dir: &Path, argv: [&str; N]) -> String {
    let output = Command::new(resolve_tool(argv[0]))
        .args(&argv[1..])
        .current_dir(current_dir)
        .output()
        .expect("spawn command");
    assert!(output.status.success(), "command failed: {:?}", argv);
    String::from_utf8(output.stdout).expect("utf-8 command output")
}

fn copy_binary_with_ldd_dependencies(binary: &Path, rootfs: &Path) {
    copy_path_into_rootfs(binary, rootfs);
    let output = Command::new(resolve_tool("ldd"))
        .arg(binary)
        .output()
        .expect("run ldd");
    assert!(
        output.status.success(),
        "ldd failed for {}",
        binary.display()
    );

    let mut copied = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((source, destination)) = parse_ldd_copy(line) {
            if copied.insert(destination.clone()) {
                copy_path_into_rootfs_as(&source, rootfs, &destination);
            }
        }
    }
}

fn parse_ldd_copy(line: &str) -> Option<(PathBuf, PathBuf)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((_, remainder)) = trimmed.split_once("=>") {
        let destination = trimmed.split_whitespace().next()?;
        let source = remainder.split_whitespace().next()?;
        if source.starts_with('/') {
            if destination.starts_with('/') {
                return Some((PathBuf::from(source), PathBuf::from(destination)));
            }
            let path = PathBuf::from(source);
            return Some((path.clone(), path));
        }
    }
    let candidate = trimmed.split_whitespace().next()?;
    if candidate.starts_with('/') {
        let path = PathBuf::from(candidate);
        return Some((path.clone(), path));
    }
    None
}

fn copy_path_into_rootfs(source: &Path, rootfs: &Path) {
    copy_path_into_rootfs_as(source, rootfs, source);
}

fn copy_path_into_rootfs_as(source: &Path, rootfs: &Path, destination_path: &Path) {
    let relative = destination_path
        .strip_prefix("/")
        .expect("absolute destination path");
    let destination = rootfs.join(relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).expect("create OCI dependency parent");
    }
    fs::copy(source, &destination).unwrap_or_else(|error| {
        panic!(
            "failed to copy {} into {}: {}",
            source.display(),
            destination.display(),
            error
        )
    });
}

fn resolve_tool(tool: &str) -> PathBuf {
    let usr_bin = PathBuf::from("/usr/bin").join(tool);
    if usr_bin.exists() {
        return usr_bin;
    }
    let bin = PathBuf::from("/bin").join(tool);
    if bin.exists() {
        return bin;
    }
    PathBuf::from(tool)
}
