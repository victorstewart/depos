// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use depos_cli::{
    collect_statuses, default_variant, host_arch, parse_depofile, parse_manifest,
    register_depofile, registry_dir_from_manifest, sync_registry, unregister_depofile,
    GlobalSystemLibs, PackageState, RegisterOptions, RequestMode, RequestSource, StageKind,
    StatusOptions, SyncOptions, UnregisterOptions,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect("owner package should materialize");

    sync_registry(&SyncOptions {
        depos_root: sandbox.depos_root(),
        manifest: sandbox
            .depos_root()
            .join("manifests/conflict_contender.cmake"),
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
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
        system_libs: Some(GlobalSystemLibs::Never),
        executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_depos"))),
    })
    .expect_err("mixed target architectures should be rejected");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains("multiple TARGET_ARCH values"),
        "{error:#}"
    );
}

fn depofile_demo() -> String {
    "NAME demo\nVERSION 1.0.0\nSYSTEM_LIBS INHERIT\nTARGET demo::demo INTERFACE include\nSOURCE URL https://example.com/demo.tar.gz\nSHA256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n".to_string()
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
        sandbox.write("depos.env.cmake", "set(DEPOS_SYSTEM_LIBS \"NEVER\")\n");
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
