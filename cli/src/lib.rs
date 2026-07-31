// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, bail, Context, Result};
#[cfg(target_os = "macos")]
use metalor::runtime::macos::{
    prepare_job as prepare_portable_worker_job, sync_worker_caches as sync_portable_worker_caches,
};
#[cfg(target_os = "windows")]
use metalor::runtime::windows::{
    prepare_job as prepare_portable_worker_job, sync_worker_caches as sync_portable_worker_caches,
};
#[cfg(target_os = "linux")]
use metalor::{
    build_unshare_reexec_command_with_backend, prepare_oci_rootfs, prepare_runtime_emulator,
    probe_rootless_userns, BindMount, ContainerRunCommand, LinuxNamespaceBackend,
    LinuxNamespaceProbe,
};
use metalor::{interpolate_braced_variables, significant_lines, valid_identifier};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use metalor::{
    BuildCellSpec, CacheSpec, CellPath, CleanupPolicy, CommandSpec, HostPath, ImportSpec,
    NetworkPolicy, WorkspaceSeed,
};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::{self, Display};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use unicode_normalization::UnicodeNormalization;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod linux_provider;

const DEFAULT_NAMESPACE: &str = "release";
const CELL_SOURCE_DIR: &str = "/work/source";
const CELL_BUILD_DIR: &str = "/work/build";
const CELL_PREFIX_DIR: &str = "/work/prefix";
const CELL_DEPS_DIR: &str = "/depos";
const CELL_TMP_DIR: &str = "/tmp";
const DEPENDENCY_ZIP_LIMITS: ZipExtractionLimits = ZipExtractionLimits {
    archive_bytes: 512 * 1024 * 1024,
    entries: 100_000,
    member_bytes: 2 * 1024 * 1024 * 1024,
    total_bytes: 8 * 1024 * 1024 * 1024,
    compression_ratio: 1_000,
    path_bytes: 4_096,
};
const DEPENDENCY_DOWNLOAD_LIMITS: UrlDownloadLimits = UrlDownloadLimits {
    bytes: DEPENDENCY_ZIP_LIMITS.archive_bytes,
    transfer_timeout: std::time::Duration::from_secs(10 * 60),
};
#[cfg(target_os = "linux")]
const DEPOS_LINUX_ISOLATION_ENV: &str = "DEPOS_LINUX_ISOLATION";

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxExecutionBackend {
    Namespaced(LinuxNamespaceBackend),
    HostStaged,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxIsolationRequest {
    Auto,
    Rootless,
    Host,
    Privileged,
}

pub fn default_depos_root_path() -> PathBuf {
    host_home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".depos")
}

fn host_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            let mut output = PathBuf::from(drive);
            output.push(path);
            Some(output)
        })
}

fn host_path_separator() -> char {
    if cfg!(windows) {
        ';'
    } else {
        ':'
    }
}

fn join_host_path_list(paths: &[String]) -> String {
    let separator = host_path_separator().to_string();
    paths.join(&separator)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PackageSystemLibs {
    Inherit,
    Never,
    Allow,
}

impl PackageSystemLibs {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "INHERIT" => Ok(Self::Inherit),
            "NEVER" => Ok(Self::Never),
            "ALLOW" => Ok(Self::Allow),
            _ => bail!("unsupported SYSTEM_LIBS value {value}"),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Inherit => "INHERIT",
            Self::Never => "NEVER",
            Self::Allow => "ALLOW",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestSource {
    Auto,
    Depo,
    System,
}

impl RequestSource {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "AUTO" => Ok(Self::Auto),
            "DEPO" => Ok(Self::Depo),
            "SYSTEM" => Ok(Self::System),
            _ => bail!("unsupported SOURCE value {value}"),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Depo => "DEPO",
            Self::System => "SYSTEM",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestMode {
    Latest,
    Exact(String),
    Minimum(String),
}

impl RequestMode {
    fn kind_str(&self) -> &'static str {
        match self {
            Self::Latest => "LATEST",
            Self::Exact(_) => "EXACT",
            Self::Minimum(_) => "MINIMUM",
        }
    }

    fn version_str(&self) -> &str {
        match self {
            Self::Latest => "",
            Self::Exact(value) | Self::Minimum(value) => value,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageRequest {
    pub name: String,
    pub namespace: String,
    pub inherit_namespace: bool,
    pub mode: RequestMode,
    pub source: RequestSource,
    pub alias: Option<String>,
}

impl PackageRequest {
    fn identity_key(&self) -> PackageKey {
        PackageKey::new(&self.name, &self.namespace)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct PackageKey {
    name: String,
    namespace: String,
}

impl PackageKey {
    fn new(name: &str, namespace: &str) -> Self {
        Self {
            name: name.to_string(),
            namespace: namespace.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum TargetKind {
    Interface,
    Static,
    Shared,
    Object,
}

impl TargetKind {
    fn cmake_imported_type(&self) -> &'static str {
        match self {
            Self::Interface => "INTERFACE",
            Self::Static => "STATIC",
            Self::Shared => "SHARED",
            Self::Object => "OBJECT",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetSpec {
    pub name: String,
    pub interface_declared: bool,
    pub include_dirs: Vec<PathBuf>,
    pub static_path: Option<PathBuf>,
    pub shared_path: Option<PathBuf>,
    pub object_path: Option<PathBuf>,
    pub link_libraries: Vec<String>,
    pub compile_definitions: Vec<String>,
    pub compile_options: Vec<String>,
    pub compile_features: Vec<String>,
}

impl TargetSpec {
    fn new(name: String) -> Self {
        Self {
            name,
            interface_declared: false,
            include_dirs: Vec::new(),
            static_path: None,
            shared_path: None,
            object_path: None,
            link_libraries: Vec::new(),
            compile_definitions: Vec::new(),
            compile_options: Vec::new(),
            compile_features: Vec::new(),
        }
    }

    fn artifact_paths(&self) -> Vec<(TargetKind, &PathBuf)> {
        let mut paths = Vec::new();
        if let Some(path) = &self.static_path {
            paths.push((TargetKind::Static, path));
        }
        if let Some(path) = &self.shared_path {
            paths.push((TargetKind::Shared, path));
        }
        if let Some(path) = &self.object_path {
            paths.push((TargetKind::Object, path));
        }
        paths
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StageKind {
    File,
    Tree,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StageSourceRoot {
    Source,
    Build,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageEntry {
    pub kind: StageKind,
    pub source_root: StageSourceRoot,
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchSpec {
    Url { url: String, sha256: Option<String> },
    Git { url: String, reference: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildRoot {
    System,
    Scratch,
    Oci(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolchainSource {
    System,
    Rootfs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildSystem {
    Cmake,
    Meson,
    Autoconf,
    Cargo,
    Manual,
}

impl BuildSystem {
    fn directive_name(&self) -> &'static str {
        match self {
            Self::Cmake => "CMAKE",
            Self::Meson => "MESON",
            Self::Autoconf => "AUTOCONF",
            Self::Cargo => "CARGO",
            Self::Manual => "MANUAL",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TargetPlatform {
    Host,
    Ios,
    IosSimulator,
}

impl TargetPlatform {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "host" => Ok(Self::Host),
            "ios" => Ok(Self::Ios),
            "ios-simulator" => Ok(Self::IosSimulator),
            other => bail!(
                "unsupported target platform '{}'; expected one of: host, ios, ios-simulator",
                other
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Ios => "ios",
            Self::IosSimulator => "ios-simulator",
        }
    }

    fn validate_host(self) -> Result<()> {
        if self == Self::Host || cfg!(target_os = "macos") {
            return Ok(());
        }
        bail!(
            "target platform '{}' requires a native macOS Depos host with the Apple SDK toolchain",
            self.as_str()
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageSpec {
    pub name: String,
    pub namespace: String,
    pub version: String,
    pub primary_target_name: Option<String>,
    pub source_subdir: Option<PathBuf>,
    pub lazy: bool,
    pub system_libs: PackageSystemLibs,
    pub artifacts: Vec<PathBuf>,
    pub targets: Vec<TargetSpec>,
    pub depends: Vec<PackageRequest>,
    pub fetch: Option<FetchSpec>,
    pub git_submodules_recursive: bool,
    pub configure: Vec<Vec<String>>,
    pub build: Vec<Vec<String>>,
    pub install: Vec<Vec<String>>,
    pub stage_entries: Vec<StageEntry>,
    pub build_root: BuildRoot,
    pub toolchain: ToolchainSource,
    pub toolchain_inputs: Vec<String>,
    pub build_arch: String,
    pub target_arch: String,
    pub supported_target_platforms: BTreeSet<TargetPlatform>,
    pub target_platform: TargetPlatform,
    build_system: BuildSystem,
    pub origin: PackageOrigin,
}

impl PackageSpec {
    pub fn package_id(&self) -> String {
        format!("{}[{}]@{}", self.name, self.namespace, self.version)
    }

    fn identity_key(&self) -> PackageKey {
        PackageKey::new(&self.name, &self.namespace)
    }

    fn primary_target_index(&self) -> Option<usize> {
        match &self.primary_target_name {
            Some(name) => self.targets.iter().position(|target| target.name == *name),
            None => (!self.targets.is_empty()).then_some(0),
        }
    }

    fn primary_target(&self) -> Option<&str> {
        self.primary_target_index()
            .map(|index| self.targets[index].name.as_str())
    }

    fn required_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.artifacts.clone();
        for target in &self.targets {
            paths.extend(target.include_dirs.clone());
            paths.extend(
                target
                    .artifact_paths()
                    .into_iter()
                    .map(|(_, path)| path.clone()),
            );
        }
        dedup_paths(paths)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageOrigin {
    Builtin,
    Local,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPackage {
    pub spec: PackageSpec,
    pub source: RequestSource,
    pub request: RequestMode,
    pub expose_default: bool,
    pub aliases: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PackageState {
    NeverRun,
    Green,
    Quarantined,
    Failed,
}

impl PackageState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::NeverRun => "never_run",
            Self::Green => "green",
            Self::Quarantined => "quarantined",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "never_run" => Ok(Self::NeverRun),
            "green" => Ok(Self::Green),
            "quarantined" => Ok(Self::Quarantined),
            "failed" => Ok(Self::Failed),
            _ => bail!("unsupported status state {value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageStatus {
    pub name: String,
    pub namespace: String,
    pub version: String,
    pub lazy: bool,
    pub system_libs: PackageSystemLibs,
    pub state: PackageState,
    pub depofile: PathBuf,
    pub message: String,
    pub source_ref: Option<String>,
    pub source_commit: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExportManifest {
    store_root: PathBuf,
    paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VariantMetadataPath {
    path: PathBuf,
    variant: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SourceProvenance {
    source_ref: Option<String>,
    source_commit: Option<String>,
    source_digest: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MaterializationState {
    store_root: PathBuf,
    depofile_hash: String,
    build_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedSource {
    source_root: PathBuf,
    provenance: SourceProvenance,
    preparation: SourcePreparationPlan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SourcePreparationPlan {
    Git {
        source_root: PathBuf,
        desired_commit: String,
        submodules_recursive: bool,
    },
    Url {
        archive_path: PathBuf,
        source_root: PathBuf,
    },
}

impl Display for PackageStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}[{}]@{} [{}] {}",
            self.name,
            self.namespace,
            self.version,
            self.state.as_str(),
            self.message
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryOutput {
    pub registry_dir: PathBuf,
    pub lock_file: PathBuf,
    pub validate_file: PathBuf,
    pub targets_file: PathBuf,
    pub selected: Vec<ResolvedPackage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncOptions {
    pub depos_root: PathBuf,
    pub manifest: PathBuf,
    pub executable: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InternalMaterializePreparedOptions {
    pub depos_root: PathBuf,
    pub name: String,
    pub namespace: String,
    pub version: String,
    pub source_root: PathBuf,
    pub store_root: PathBuf,
    pub executable: PathBuf,
    pub target_platform: TargetPlatform,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterOptions {
    pub depos_root: PathBuf,
    pub file: PathBuf,
    pub namespace: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnregisterOptions {
    pub depos_root: PathBuf,
    pub name: String,
    pub namespace: String,
    pub version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusOptions {
    pub depos_root: PathBuf,
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub version: Option<String>,
    pub refresh: bool,
}

pub fn default_variant() -> String {
    let arch = host_arch();
    variant_for_target(&arch, TargetPlatform::Host).expect("host architecture is normalized")
}

fn default_namespace() -> String {
    DEFAULT_NAMESPACE.to_string()
}

pub fn host_arch() -> String {
    normalize_arch_name(std::env::consts::ARCH).expect("unsupported host architecture")
}

fn variant_for_target(target_arch: &str, target_platform: TargetPlatform) -> Result<String> {
    let arch = normalize_arch_name(target_arch)?;
    match target_platform {
        TargetPlatform::Host => Ok(format!("{arch}-{arch}_v1")),
        _ => Ok(format!("{arch}-{arch}-{}_v1", target_platform.as_str())),
    }
}

fn package_relative_store_root(spec: &PackageSpec) -> PathBuf {
    PathBuf::from(&spec.name)
        .join(&spec.namespace)
        .join(&spec.version)
}

fn package_store_root(depos_root: &Path, variant: &str, spec: &PackageSpec) -> PathBuf {
    depos_root
        .join("store")
        .join(variant)
        .join(package_relative_store_root(spec))
}

fn variant_package_store_root(
    depos_root: &Path,
    variant: &str,
    name: &str,
    namespace: &str,
    version: &str,
) -> PathBuf {
    depos_root
        .join("store")
        .join(variant)
        .join(name)
        .join(namespace)
        .join(version)
}

fn store_roots_match(left: &Path, right: &Path) -> Result<bool> {
    if left == right {
        return Ok(true);
    }
    if !left.exists() || !right.exists() {
        return Ok(false);
    }
    Ok(canonical_path(left)? == canonical_path(right)?)
}

fn package_store_root_for_selected(
    depos_root: &Path,
    variant: &str,
    package: &ResolvedPackage,
) -> PathBuf {
    package_store_root(depos_root, variant, &package.spec)
}

fn variant_for_selected_packages(
    selected: &[ResolvedPackage],
    target_platform: TargetPlatform,
) -> Result<String> {
    let mut targets = BTreeMap::<(String, TargetPlatform), Vec<String>>::new();
    for package in selected {
        targets
            .entry((
                package.spec.target_arch.clone(),
                package.spec.target_platform,
            ))
            .or_default()
            .push(package.spec.package_id());
    }
    if targets.is_empty() {
        return variant_for_target(&host_arch(), target_platform);
    }
    if targets.len() != 1 {
        let details = targets
            .into_iter()
            .map(|((arch, platform), packages)| {
                format!("{arch}/{}: {}", platform.as_str(), packages.join(", "))
            })
            .collect::<Vec<_>>()
            .join("; ");
        bail!(
            "selected packages span multiple target architecture/platform values, which cannot share one registry/store variant: {}",
            details
        );
    }
    let ((target_arch, target_platform), _) = targets.into_iter().next().unwrap();
    variant_for_target(&target_arch, target_platform)
}

fn normalize_arch_name(value: &str) -> Result<String> {
    match value {
        "x86_64" | "amd64" => Ok("x86_64".to_string()),
        "aarch64" | "arm64" => Ok("aarch64".to_string()),
        "riscv64" => Ok("riscv64".to_string()),
        other => bail!("unsupported architecture {}", other),
    }
}

#[allow(dead_code)]
fn linux_gnu_target_triple(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        "riscv64" => "riscv64gc-unknown-linux-gnu",
        other => panic!("unsupported linux gnu target triple architecture {}", other),
    }
}

#[allow(dead_code)]
fn linux_gnu_toolchain_prefix(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "x86_64-linux-gnu",
        "aarch64" => "aarch64-linux-gnu",
        "riscv64" => "riscv64-linux-gnu",
        other => panic!(
            "unsupported linux gnu toolchain prefix architecture {}",
            other
        ),
    }
}

#[allow(dead_code)]
fn debian_crossbuild_package(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "crossbuild-essential-amd64",
        "aarch64" => "crossbuild-essential-arm64",
        "riscv64" => "crossbuild-essential-riscv64",
        other => panic!("unsupported debian crossbuild architecture {}", other),
    }
}

#[allow(dead_code)]
fn cargo_target_env_fragment(triple: &str) -> String {
    triple.replace('-', "_").to_ascii_uppercase()
}

pub fn registry_dir_from_manifest(depos_root: &Path, manifest: &Path) -> Result<PathBuf> {
    registry_dir_from_manifest_for_target_platform(depos_root, manifest, TargetPlatform::Host)
}

pub fn registry_dir_from_manifest_for_target_platform(
    depos_root: &Path,
    manifest: &Path,
    target_platform: TargetPlatform,
) -> Result<PathBuf> {
    target_platform.validate_host()?;
    let depos_root = resolve_depos_root(depos_root)?;
    let requests = parse_manifest(manifest)?;
    rebuild_embedded_depofile_catalog(&depos_root, &requests)?;
    let catalog = load_catalog(&depos_root)?;
    let mut selected = resolve_requests(&catalog, &requests)?;
    apply_target_platform(&mut selected, target_platform)?;
    let variant = variant_for_selected_packages(&selected, target_platform)?;
    let profile = manifest_profile(manifest)?;
    Ok(depos_root.join("registry").join(variant).join(profile))
}

pub fn sync_registry(options: &SyncOptions) -> Result<RegistryOutput> {
    sync_registry_for_target_platform(options, TargetPlatform::Host)
}

pub fn sync_registry_for_target_platform(
    options: &SyncOptions,
    target_platform: TargetPlatform,
) -> Result<RegistryOutput> {
    target_platform.validate_host()?;
    let depos_root = resolve_depos_root(&options.depos_root).with_context(|| {
        format!(
            "failed to access depos root {}",
            options.depos_root.display()
        )
    })?;
    let manifest = canonical_path(&options.manifest)
        .with_context(|| format!("failed to access manifest {}", options.manifest.display()))?;
    let executable = resolve_depos_executable(options.executable.as_deref())?;

    let requests = parse_manifest(&manifest)?;
    rebuild_embedded_depofile_catalog(&depos_root, &requests)?;
    let catalog = load_catalog(&depos_root)?;
    let mut selected = resolve_requests(&catalog, &requests)?;
    apply_target_platform(&mut selected, target_platform)?;
    let variant = variant_for_selected_packages(&selected, target_platform)?;
    let variant_root = depos_root.join("store").join(&variant);
    fs::create_dir_all(&variant_root)
        .with_context(|| format!("failed to create {}", variant_root.display()))?;
    let registry_dir = depos_root
        .join("registry")
        .join(&variant)
        .join(manifest_profile(&manifest)?);
    fs::create_dir_all(&registry_dir).with_context(|| {
        format!(
            "failed to create registry directory {}",
            registry_dir.display()
        )
    })?;
    materialize_local_packages(&depos_root, &variant, &selected, &executable)?;
    ensure_builtin_package_roots(&depos_root, &variant, &selected)?;
    validate_materialized_packages(&depos_root, &variant, &selected)?;

    let validate_file = registry_dir.join("validate.cmake");
    let targets_file = registry_dir.join("targets.cmake");
    let lock_file = registry_dir.join("lock.cmake");

    fs::write(
        &validate_file,
        render_validate_cmake(&depos_root, &variant, &selected),
    )
    .with_context(|| format!("failed to write {}", validate_file.display()))?;
    fs::write(
        &targets_file,
        render_targets_cmake(&depos_root, &variant, &validate_file, &selected)?,
    )
    .with_context(|| format!("failed to write {}", targets_file.display()))?;
    fs::write(
        &lock_file,
        render_lock_cmake(&depos_root, &manifest, &variant, &registry_dir, &selected),
    )
    .with_context(|| format!("failed to write {}", lock_file.display()))?;

    Ok(RegistryOutput {
        registry_dir,
        lock_file,
        validate_file,
        targets_file,
        selected,
    })
}

fn apply_target_platform(
    selected: &mut [ResolvedPackage],
    target_platform: TargetPlatform,
) -> Result<()> {
    for package in selected {
        ensure_target_platform_supported(&package.spec, target_platform)?;
        package.spec.target_platform = target_platform;
    }
    Ok(())
}

fn ensure_target_platform_supported(
    spec: &PackageSpec,
    target_platform: TargetPlatform,
) -> Result<()> {
    if spec.supported_target_platforms.contains(&target_platform) {
        return Ok(());
    }
    bail!(
        "package '{}' does not declare target platform '{}' in TARGET_PLATFORMS",
        spec.package_id(),
        target_platform.as_str()
    )
}

#[cfg(target_os = "linux")]
pub fn internal_materialize_prepared(options: &InternalMaterializePreparedOptions) -> Result<()> {
    let depos_root = resolve_depos_root(&options.depos_root).with_context(|| {
        format!(
            "failed to access provider depos root {}",
            options.depos_root.display()
        )
    })?;
    ensure_package_name(&options.name)?;
    ensure_namespace_name(&options.namespace)?;
    ensure_version_name(&options.version)?;
    let depofile = resolve_registered_depofile_path(
        &depos_root,
        &options.name,
        &options.namespace,
        &options.version,
    )?;
    let mut spec = parse_registered_depofile(
        &depofile,
        &options.name,
        &options.namespace,
        &options.version,
    )?;
    spec.target_platform = options.target_platform;
    ensure_target_platform_supported(&spec, options.target_platform)?;
    let source_root = canonical_path(&options.source_root).with_context(|| {
        format!(
            "failed to access source root {}",
            options.source_root.display()
        )
    })?;
    let store_root = options.store_root.clone();
    if let Some(parent) = store_root.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::create_dir_all(&store_root)
        .with_context(|| format!("failed to create {}", store_root.display()))?;
    let mut log = String::new();
    log.push_str(&format!(
        "provider materializing {} from prepared source {}\n",
        spec.package_id(),
        source_root.display()
    ));
    let result = if spec.configure.is_empty()
        && spec.build.is_empty()
        && spec.install.is_empty()
        && spec.stage_entries.is_empty()
    {
        copy_declared_exports(&depos_root, &source_root, &store_root, &spec, &mut log)
    } else {
        execute_command_pipeline(
            &depos_root,
            &store_root,
            &spec,
            &options.executable,
            &source_root,
            &mut log,
        )
    };
    match result {
        Ok(_) => {
            log.push_str("provider materialization complete\n");
            eprint!("{log}");
            Ok(())
        }
        Err(error) => {
            eprint!("{log}");
            Err(error)
        }
    }
}

fn materialize_local_packages(
    depos_root: &Path,
    variant: &str,
    selected: &[ResolvedPackage],
    executable: &Path,
) -> Result<()> {
    for layer in local_materialization_layers(depos_root, selected)? {
        if layer.len() == 1 {
            materialize_one_local_package(depos_root, variant, layer[0], executable)?;
            continue;
        }

        let mut first_error = None;
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for package in &layer {
                let package = *package;
                handles.push((
                    package.spec.package_id(),
                    scope.spawn(move || {
                        materialize_one_local_package(depos_root, variant, package, executable)
                    }),
                ));
            }

            for (package_id, handle) in handles {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        if first_error.is_none() {
                            first_error =
                                Some(error.context(format!("failed to materialize {package_id}")));
                        }
                    }
                    Err(_) => {
                        if first_error.is_none() {
                            first_error = Some(anyhow!(
                                "local materialization worker panicked for {package_id}"
                            ));
                        }
                    }
                }
            }
        });
        if let Some(error) = first_error {
            return Err(error);
        }
    }
    seed_builtin_package_roots(depos_root, variant, selected)?;
    Ok(())
}

fn materialize_one_local_package(
    depos_root: &Path,
    variant: &str,
    package: &ResolvedPackage,
    executable: &Path,
) -> Result<()> {
    let store_root = package_store_root(depos_root, variant, &package.spec);
    fs::create_dir_all(&store_root)
        .with_context(|| format!("failed to create {}", store_root.display()))?;
    match materialize_local_package(depos_root, variant, &store_root, &package.spec, executable) {
        Ok(message) => {
            write_materialization_status(depos_root, &package.spec, PackageState::Green, message)?;
            Ok(())
        }
        Err(error) => {
            write_materialization_status(
                depos_root,
                &package.spec,
                PackageState::Failed,
                error.to_string(),
            )?;
            Err(error)
        }
    }
}

fn local_materialization_layers<'a>(
    depos_root: &Path,
    selected: &'a [ResolvedPackage],
) -> Result<Vec<Vec<&'a ResolvedPackage>>> {
    let local_packages = selected
        .iter()
        .filter(|package| package.spec.origin == PackageOrigin::Local)
        .map(|package| (package.spec.package_id(), package))
        .collect::<BTreeMap<_, _>>();
    let mut depths = BTreeMap::<String, usize>::new();
    let mut visiting = BTreeSet::new();

    for package in selected {
        compute_local_materialization_depth(
            depos_root,
            package,
            &local_packages,
            &mut visiting,
            &mut depths,
        )?;
    }

    let mut layers = Vec::<Vec<&ResolvedPackage>>::new();
    for package in local_packages.values() {
        let depth = *depths.get(&package.spec.package_id()).with_context(|| {
            format!(
                "missing materialization depth for {}",
                package.spec.package_id()
            )
        })?;
        if layers.len() <= depth {
            layers.resize_with(depth + 1, Vec::new);
        }
        layers[depth].push(*package);
    }
    for layer in &mut layers {
        layer.sort_by(|left, right| left.spec.package_id().cmp(&right.spec.package_id()));
    }
    layers.retain(|layer| !layer.is_empty());
    Ok(layers)
}

fn compute_local_materialization_depth<'a>(
    depos_root: &Path,
    package: &'a ResolvedPackage,
    local_packages: &BTreeMap<String, &'a ResolvedPackage>,
    visiting: &mut BTreeSet<String>,
    depths: &mut BTreeMap<String, usize>,
) -> Result<usize> {
    if package.spec.origin != PackageOrigin::Local {
        return Ok(0);
    }

    let package_id = package.spec.package_id();
    if let Some(depth) = depths.get(&package_id) {
        return Ok(*depth);
    }
    if !visiting.insert(package_id.clone()) {
        bail!(
            "local package dependency cycle detected while materializing {}",
            package_id
        );
    }

    let mut max_dependency_depth = 0usize;
    for dependency in resolve_dependency_specs(depos_root, &package.spec)? {
        if let Some(local_dependency) = local_packages.get(&dependency.package_id()) {
            let dependency_depth = compute_local_materialization_depth(
                depos_root,
                local_dependency,
                local_packages,
                visiting,
                depths,
            )?;
            max_dependency_depth = max_dependency_depth.max(dependency_depth + 1);
        }
    }

    visiting.remove(&package_id);
    depths.insert(package_id, max_dependency_depth);
    Ok(max_dependency_depth)
}

fn ensure_builtin_package_roots(
    depos_root: &Path,
    variant: &str,
    selected: &[ResolvedPackage],
) -> Result<()> {
    let legacy_root = depos_root.join("store").join(variant);
    for package in selected {
        if package.spec.origin != PackageOrigin::Builtin {
            continue;
        }
        let package_root = package_store_root_for_selected(depos_root, variant, package);
        if package_exports_present(&package.spec, &package_root)? {
            continue;
        }
        if !package_exports_present(&package.spec, &legacy_root)? {
            continue;
        }
        migrate_builtin_from_legacy(&legacy_root, &package_root, &package.spec)?;
    }
    Ok(())
}

fn migrate_builtin_from_legacy(
    legacy_root: &Path,
    package_root: &Path,
    spec: &PackageSpec,
) -> Result<()> {
    let mut log = String::new();
    match spec.name.as_str() {
        "bitsery" => {
            copy_export_path(
                legacy_root,
                package_root,
                Path::new("include/bitsery"),
                true,
                &mut log,
            )?;
        }
        "itoa" => {
            copy_export_path(
                legacy_root,
                package_root,
                Path::new("include/itoa"),
                true,
                &mut log,
            )?;
        }
        "zlib" => {
            for relative_path in [
                Path::new("include/zlib.h"),
                Path::new("include/zconf.h"),
                Path::new(builtin_zlib_static_library_path()),
            ] {
                copy_export_path(legacy_root, package_root, relative_path, false, &mut log)?;
            }
        }
        other => bail!(
            "builtin package '{}' is present only in the legacy flat store, but no migration strategy is defined",
            other
        ),
    }
    Ok(())
}

fn seed_builtin_package_roots(
    depos_root: &Path,
    variant: &str,
    selected: &[ResolvedPackage],
) -> Result<()> {
    let variant_root = depos_root.join("store").join(variant);
    for package in selected {
        if package.spec.origin != PackageOrigin::Builtin {
            continue;
        }
        let package_root = package_store_root(depos_root, variant, &package.spec);
        if package_exports_present(&package.spec, &package_root)? {
            continue;
        }
        if !package_exports_present(&package.spec, &variant_root)? {
            continue;
        }
        fs::create_dir_all(&package_root)
            .with_context(|| format!("failed to create {}", package_root.display()))?;
        let mut log = String::new();
        for artifact in &package.spec.artifacts {
            copy_export_path(&variant_root, &package_root, artifact, false, &mut log)?;
        }
        for target in &package.spec.targets {
            for include_dir in &target.include_dirs {
                copy_export_path(&variant_root, &package_root, include_dir, true, &mut log)?;
            }
            for (_, path) in target.artifact_paths() {
                copy_export_path(&variant_root, &package_root, path, false, &mut log)?;
            }
        }
    }
    Ok(())
}

fn resolve_dependency_specs(depos_root: &Path, spec: &PackageSpec) -> Result<Vec<PackageSpec>> {
    let catalog = load_catalog(depos_root)?;
    let by_key = group_catalog_by_key(&catalog);
    let mut dependencies = Vec::new();
    for dependency in &spec.depends {
        let candidates = by_key.get(&dependency.identity_key()).with_context(|| {
            format!(
                "package '{}' depends on missing package '{}[{}]'",
                spec.package_id(),
                dependency.name,
                dependency.namespace
            )
        })?;
        let mut selected = select_package(candidates, &dependency.mode).with_context(|| {
            format!(
                "package '{}' could not resolve dependency '{}[{}]'",
                spec.package_id(),
                dependency.name,
                dependency.namespace
            )
        })?;
        selected.target_platform = spec.target_platform;
        dependencies.push(selected);
    }
    Ok(dependencies)
}

fn materialize_local_package(
    depos_root: &Path,
    variant: &str,
    store_root: &Path,
    spec: &PackageSpec,
    executable: &Path,
) -> Result<String> {
    let mut log = String::new();
    log.push_str(&format!("materializing {}\n", spec.package_id()));
    let previous_exports = read_export_manifest(
        depos_root,
        &spec.name,
        &spec.namespace,
        &spec.version,
        variant,
    )?;
    let previous_state = read_materialization_state(
        depos_root,
        &spec.name,
        &spec.namespace,
        &spec.version,
        variant,
    )?;
    let depofile_hash = registered_depofile_hash(depos_root, spec)?;
    let dependency_keys = dependency_materialization_keys(depos_root, spec, variant)?;
    let resolved_source = resolve_package_source(depos_root, spec, &mut log)?;
    let build_key = materialization_build_key(
        &depofile_hash,
        &resolved_source.provenance,
        &dependency_keys,
    );
    if materialization_is_current(
        previous_state.as_ref(),
        previous_exports.as_ref(),
        store_root,
        &depofile_hash,
        &build_key,
        spec,
    )? {
        log.push_str("materialization already up to date\n");
        write_source_provenance(depos_root, spec, &resolved_source.provenance)?;
        write_materialization_state(
            depos_root,
            spec,
            variant,
            store_root,
            &depofile_hash,
            &build_key,
        )?;
        write_materialization_log(depos_root, spec, &log)?;
        return Ok(format!(
            "materialization already up to date under {}",
            store_root.display()
        ));
    }

    prepare_package_source(&resolved_source.preparation, &mut log)?;
    let source_root = resolve_source_root(&resolved_source.source_root, spec)?;
    write_source_provenance(depos_root, spec, &resolved_source.provenance)?;
    let materialize_result = if spec.configure.is_empty()
        && spec.build.is_empty()
        && spec.install.is_empty()
        && spec.stage_entries.is_empty()
    {
        copy_declared_exports(depos_root, &source_root, store_root, spec, &mut log)
    } else {
        execute_command_pipeline(
            depos_root,
            store_root,
            spec,
            executable,
            &source_root,
            &mut log,
        )
    };
    let exported_paths = match materialize_result {
        Ok(paths) => paths,
        Err(error) => {
            write_materialization_log(depos_root, spec, &log)?;
            return Err(error.context(format!(
                "materialization log for {}:\n{}",
                spec.package_id(),
                log
            )));
        }
    };

    reconcile_export_manifest(
        store_root,
        previous_exports.as_ref(),
        &exported_paths,
        &mut log,
    )?;
    write_export_manifest(depos_root, spec, variant, store_root, &exported_paths)?;
    write_materialization_state(
        depos_root,
        spec,
        variant,
        store_root,
        &depofile_hash,
        &build_key,
    )?;

    if !package_exports_present(spec, store_root)? {
        let message = format!(
            "materialization completed but declared exports are still missing under {}",
            store_root.display()
        );
        write_materialization_log(depos_root, spec, &log)?;
        write_materialization_status(depos_root, spec, PackageState::Failed, message.clone())?;
        bail!("{message}");
    }

    log.push_str("materialization complete\n");
    write_materialization_log(depos_root, spec, &log)?;
    Ok(format!(
        "all declared exports are present under {}",
        store_root.display()
    ))
}

fn resolve_source_root(fetched_source_root: &Path, spec: &PackageSpec) -> Result<PathBuf> {
    match &spec.source_subdir {
        Some(relative_path) => {
            let resolved = fetched_source_root.join(relative_path);
            if !resolved.exists() {
                bail!(
                    "package '{}' declares SOURCE_SUBDIR '{}', but '{}' does not exist",
                    spec.package_id(),
                    relative_path.display(),
                    resolved.display()
                );
            }
            ensure_resolved_path_within_root(
                &resolved,
                fetched_source_root,
                &format!(
                    "package '{}' SOURCE_SUBDIR '{}'",
                    spec.package_id(),
                    relative_path.display()
                ),
            )?;
            Ok(canonical_path(&resolved)?)
        }
        None => Ok(fetched_source_root.to_path_buf()),
    }
}

fn resolve_package_source(
    depos_root: &Path,
    spec: &PackageSpec,
    log: &mut String,
) -> Result<ResolvedSource> {
    let fetch = spec
        .fetch
        .as_ref()
        .with_context(|| format!("package '{}' has no SOURCE directive", spec.package_id()))?;
    let download_root = depos_root
        .join("downloads")
        .join(&spec.name)
        .join(&spec.namespace)
        .join(&spec.version);
    let source_root = download_root.join("src");
    fs::create_dir_all(&download_root)
        .with_context(|| format!("failed to create {}", download_root.display()))?;

    match fetch {
        FetchSpec::Git { url, reference } => {
            let desired_commit = resolve_git_source(
                url,
                reference,
                spec.git_submodules_recursive,
                &source_root,
                log,
            )?;
            Ok(ResolvedSource {
                source_root: source_root.clone(),
                provenance: SourceProvenance {
                    source_ref: Some(reference.clone()),
                    source_commit: Some(desired_commit.clone()),
                    source_digest: None,
                },
                preparation: SourcePreparationPlan::Git {
                    source_root,
                    desired_commit,
                    submodules_recursive: spec.git_submodules_recursive,
                },
            })
        }
        FetchSpec::Url { url, sha256 } => {
            let archive_name = url
                .rsplit('/')
                .next()
                .filter(|value| !value.is_empty())
                .unwrap_or("source.tar");
            let archive_path = download_root.join(archive_name);
            let archive_digest = resolve_url_source(url, sha256.as_deref(), &archive_path, log)?;
            Ok(ResolvedSource {
                source_root,
                provenance: SourceProvenance {
                    source_ref: Some(url.clone()),
                    source_commit: None,
                    source_digest: Some(archive_digest),
                },
                preparation: SourcePreparationPlan::Url {
                    archive_path,
                    source_root: download_root.join("src"),
                },
            })
        }
    }
}

fn prepare_package_source(preparation: &SourcePreparationPlan, log: &mut String) -> Result<()> {
    match preparation {
        SourcePreparationPlan::Git {
            source_root,
            desired_commit,
            submodules_recursive,
        } => prepare_git_source(source_root, desired_commit, *submodules_recursive, log),
        SourcePreparationPlan::Url {
            archive_path,
            source_root,
        } => prepare_url_source(archive_path, source_root, log),
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_isolation_request() -> Result<LinuxIsolationRequest> {
    match std::env::var(DEPOS_LINUX_ISOLATION_ENV) {
        Ok(value) => match value.as_str() {
            "" | "auto" => Ok(LinuxIsolationRequest::Auto),
            "rootless" => Ok(LinuxIsolationRequest::Rootless),
            "host" => Ok(LinuxIsolationRequest::Host),
            "privileged" => Ok(LinuxIsolationRequest::Privileged),
            other => bail!(
                "unsupported {DEPOS_LINUX_ISOLATION_ENV}={other}; expected one of: auto, rootless, host, privileged"
            ),
        },
        Err(std::env::VarError::NotPresent) => Ok(LinuxIsolationRequest::Auto),
        Err(std::env::VarError::NotUnicode(value)) => bail!(
            "{DEPOS_LINUX_ISOLATION_ENV} must be valid UTF-8, got {}",
            value.to_string_lossy()
        ),
    }
}

#[cfg(target_os = "linux")]
fn strict_root_required(spec: &PackageSpec) -> bool {
    matches!(spec.build_root, BuildRoot::Scratch | BuildRoot::Oci(_))
}

#[cfg(target_os = "linux")]
fn build_root_label(root: &BuildRoot) -> String {
    match root {
        BuildRoot::System => "BUILD_ROOT SYSTEM".to_string(),
        BuildRoot::Scratch => "BUILD_ROOT SCRATCH".to_string(),
        BuildRoot::Oci(reference) => format!("BUILD_ROOT OCI {reference}"),
    }
}

#[cfg(target_os = "linux")]
fn select_linux_backend(
    spec: &PackageSpec,
    disable_network: bool,
    mode: LinuxIsolationRequest,
    log: &mut String,
) -> Result<LinuxExecutionBackend> {
    let strict = strict_root_required(spec);
    match mode {
        LinuxIsolationRequest::Rootless => {
            let probe = probe_rootless_userns(disable_network);
            if probe.ok {
                log.push_str("using rootless Linux namespace backend\n");
                return Ok(LinuxExecutionBackend::Namespaced(
                    LinuxNamespaceBackend::RootlessUser,
                ));
            }
            bail!(
                "rootless Linux namespaces unavailable:\n{}",
                render_linux_namespace_probe_failure(&probe)
            );
        }
        LinuxIsolationRequest::Host => {
            if strict {
                bail!(
                    "host-staged backend is only valid for BUILD_ROOT SYSTEM; {} requires Linux namespace isolation",
                    build_root_label(&spec.build_root)
                );
            }
            log.push_str("using host-staged BUILD_ROOT SYSTEM backend\n");
            return Ok(LinuxExecutionBackend::HostStaged);
        }
        LinuxIsolationRequest::Privileged => {
            log.push_str("using privileged Linux namespace backend\n");
            return Ok(LinuxExecutionBackend::Namespaced(
                LinuxNamespaceBackend::Privileged,
            ));
        }
        LinuxIsolationRequest::Auto => {}
    }

    let rootless = probe_rootless_userns(disable_network);
    if rootless.ok {
        log.push_str("using rootless Linux namespace backend\n");
        return Ok(LinuxExecutionBackend::Namespaced(
            LinuxNamespaceBackend::RootlessUser,
        ));
    }

    log.push_str("rootless Linux namespace backend unavailable:\n");
    log.push_str(&render_linux_namespace_probe_failure(&rootless));
    log.push('\n');
    if !strict {
        log.push_str("falling back to host-staged BUILD_ROOT SYSTEM backend\n");
        return Ok(LinuxExecutionBackend::HostStaged);
    }

    bail!(
        "{} requires Linux namespace isolation.\nrootless user namespaces are unavailable:\n{}\nhost-staged fallback is not valid for {}.\nEnable unprivileged user namespaces, or run with {DEPOS_LINUX_ISOLATION_ENV}=privileged from a process with CAP_SYS_ADMIN.",
        build_root_label(&spec.build_root),
        render_linux_namespace_probe_failure(&rootless),
        build_root_label(&spec.build_root)
    );
}

#[cfg(target_os = "linux")]
fn render_linux_namespace_probe_failure(probe: &LinuxNamespaceProbe) -> String {
    let mut message = String::new();
    message.push_str("  ");
    message.push_str(&render_linux_namespace_probe_command(probe.disable_network));
    if let Some(status) = probe.status {
        message.push_str(&format!("\n  exited with status {status}"));
    } else {
        message.push_str("\n  failed to spawn");
    }
    let stderr = probe.stderr.trim();
    if !stderr.is_empty() {
        message.push_str(": ");
        message.push_str(stderr);
    }
    message
}

#[cfg(target_os = "linux")]
fn render_linux_namespace_probe_command(disable_network: bool) -> String {
    let mut parts = vec![
        "unshare",
        "--fork",
        "--user",
        "--map-root-user",
        "--pid",
        "--mount",
        "--uts",
        "--ipc",
    ];
    if disable_network {
        parts.push("--net");
    }
    parts.extend(["--", "true"]);
    parts.join(" ")
}

#[cfg(target_os = "linux")]
fn rootless_runtime_failure_allows_system_fallback(error: &str, execution_log: &str) -> bool {
    let combined = format!("{error}\n{execution_log}").to_ascii_lowercase();
    let namespace_failure = combined.contains("bind mount ")
        || combined.contains("mount proc for container command")
        || combined.contains("mount binfmt_misc for container command")
        || combined.contains("failed to chroot into")
        || combined.contains("host mount namespace")
        || combined.contains("metalor_private_ns")
        || combined.contains("failed to spawn isolated depos internal-run command");
    let permission_failure = combined.contains("operation not permitted")
        || combined.contains("permission denied")
        || combined.contains("not permitted");
    namespace_failure && permission_failure
}

#[cfg(target_os = "linux")]
fn execute_command_pipeline(
    depos_root: &Path,
    store_root: &Path,
    spec: &PackageSpec,
    executable: &Path,
    source_root: &Path,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    validate_supported_command_pipeline(spec)?;
    let mode = parse_linux_isolation_request()?;
    let disable_network = false;
    let backend = select_linux_backend(spec, disable_network, mode, log)?;
    match backend {
        LinuxExecutionBackend::Namespaced(namespace_backend) => {
            let log_start = log.len();
            match execute_linux_namespaced_command_pipeline(
                depos_root,
                store_root,
                spec,
                executable,
                source_root,
                namespace_backend,
                log,
            ) {
                Ok(paths) => Ok(paths),
                Err(error)
                    if mode == LinuxIsolationRequest::Auto
                        && namespace_backend == LinuxNamespaceBackend::RootlessUser
                        && !strict_root_required(spec)
                        && rootless_runtime_failure_allows_system_fallback(
                            &format!("{error:#}"),
                            &log[log_start..],
                        ) =>
                {
                    log.push_str(&format!(
                        "rootless Linux namespace backend failed during execution: {error:#}\n"
                    ));
                    log.push_str("falling back to host-staged BUILD_ROOT SYSTEM backend\n");
                    execute_linux_host_staged_system_pipeline(
                        depos_root,
                        store_root,
                        spec,
                        source_root,
                        log,
                    )
                }
                Err(error) => Err(error),
            }
        }
        LinuxExecutionBackend::HostStaged => execute_linux_host_staged_system_pipeline(
            depos_root,
            store_root,
            spec,
            source_root,
            log,
        ),
    }
}

#[cfg(target_os = "linux")]
fn execute_linux_namespaced_command_pipeline(
    depos_root: &Path,
    store_root: &Path,
    spec: &PackageSpec,
    executable: &Path,
    source_root: &Path,
    namespace_backend: LinuxNamespaceBackend,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    validate_supported_command_pipeline(spec)?;
    let host = host_arch();
    let variant = variant_for_target(&spec.target_arch, spec.target_platform)?;
    let variant_root = depos_root.join("store").join(&variant);
    let dependency_specs = resolve_dependency_specs(depos_root, spec)?;

    let runtime_root_prefix = depos_root.join(".run").join("metalor-runtime");
    let runtime_package_root = runtime_root_prefix
        .join(&spec.name)
        .join(&spec.namespace)
        .join(&spec.version);
    if runtime_package_root.exists() {
        fs::remove_dir_all(&runtime_package_root)
            .with_context(|| format!("failed to remove {}", runtime_package_root.display()))?;
    }
    fs::create_dir_all(&runtime_package_root)
        .with_context(|| format!("failed to create {}", runtime_package_root.display()))?;
    let container_root = prepare_command_container_root(
        depos_root,
        &runtime_root_prefix,
        &runtime_package_root,
        spec,
        log,
    )?;
    let work_source = container_root.join("work/source");
    let work_build = container_root.join("work/build");
    let work_prefix = container_root.join("work/prefix");
    let work_tmp = container_root.join("tmp");
    fs::create_dir_all(&work_source)
        .with_context(|| format!("failed to create {}", work_source.display()))?;
    fs::create_dir_all(&work_build)
        .with_context(|| format!("failed to create {}", work_build.display()))?;
    fs::create_dir_all(&work_prefix)
        .with_context(|| format!("failed to create {}", work_prefix.display()))?;
    fs::create_dir_all(&work_tmp)
        .with_context(|| format!("failed to create {}", work_tmp.display()))?;

    let variables = build_command_variables(spec, &dependency_specs)?;
    let base_env = build_command_environment(spec, &dependency_specs);
    let mounts = build_command_mounts(spec, source_root, &variant_root)?;
    let emulator = prepare_command_emulator(&container_root, &host, spec, log)?;
    let phase_context = PhaseExecutionContext {
        spec,
        executable,
        runtime_root_prefix: &runtime_root_prefix,
        container_root: &container_root,
        mounts: &mounts,
        emulator: emulator.as_deref(),
        base_env: &base_env,
        variables: &variables,
        namespace_backend,
    };

    let provider_bootstrap = linux_provider_bootstrap_commands(spec)?;
    execute_command_phase(
        &phase_context,
        default_phase_cwd(spec, "CONFIGURE"),
        "PROVIDER_BOOTSTRAP",
        &provider_bootstrap,
        log,
    )?;

    execute_command_phase(
        &phase_context,
        default_phase_cwd(spec, "CONFIGURE"),
        "CONFIGURE",
        &spec.configure,
        log,
    )?;
    execute_command_phase(
        &phase_context,
        default_phase_cwd(spec, "BUILD"),
        "BUILD",
        &spec.build,
        log,
    )?;
    execute_command_phase(
        &phase_context,
        default_phase_cwd(spec, "INSTALL"),
        "INSTALL",
        &spec.install,
        log,
    )?;

    apply_stage_entries(
        source_root,
        &work_build,
        &work_prefix,
        &spec.stage_entries,
        log,
    )?;

    if spec.build_system == BuildSystem::Manual
        && spec.install.is_empty()
        && spec.stage_entries.is_empty()
    {
        return copy_declared_exports_from_candidates(
            depos_root,
            &[
                ExportCandidateRoot {
                    label: "build",
                    root: &work_build,
                },
                ExportCandidateRoot {
                    label: "source",
                    root: source_root,
                },
            ],
            store_root,
            spec,
            log,
        );
    }

    copy_declared_exports(depos_root, &work_prefix, store_root, spec, log)
}

#[cfg(target_os = "linux")]
fn execute_linux_host_staged_system_pipeline(
    depos_root: &Path,
    store_root: &Path,
    spec: &PackageSpec,
    source_root: &Path,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    validate_supported_command_pipeline(spec)?;
    if strict_root_required(spec) {
        bail!("host-staged backend is only valid for BUILD_ROOT SYSTEM");
    }

    let variant = variant_for_target(&spec.target_arch, spec.target_platform)?;
    let variant_root = depos_root.join("store").join(&variant);
    let dependency_specs = resolve_dependency_specs(depos_root, spec)?;
    let runtime_root_prefix = depos_root.join(".run").join("metalor-runtime");
    let runtime_package_root = runtime_root_prefix
        .join(&spec.name)
        .join(&spec.namespace)
        .join(&spec.version);
    if runtime_package_root.exists() {
        fs::remove_dir_all(&runtime_package_root)
            .with_context(|| format!("failed to remove {}", runtime_package_root.display()))?;
    }
    fs::create_dir_all(&runtime_package_root)
        .with_context(|| format!("failed to create {}", runtime_package_root.display()))?;

    let job_root = runtime_package_root.join("job");
    let cache_root = runtime_package_root.join("cache");
    let cache_build = cache_root.join("build");
    let cache_prefix = cache_root.join("prefix");
    let cache_tmp = cache_root.join("tmp");
    let build_cell = build_portable_command_cell(
        &runtime_package_root,
        &cache_build,
        &cache_prefix,
        &cache_tmp,
        &variant_root,
        source_root,
        &dependency_specs,
    )?;
    prepare_host_staged_build_cell(&job_root, &build_cell, log)?;
    let paths = portable_command_paths(&job_root)?;
    let variables = build_portable_command_variables(spec, &dependency_specs, &paths)?;
    let base_env = build_portable_command_environment(spec, &dependency_specs, &paths);

    execute_portable_command_phase(
        spec,
        &paths,
        &base_env,
        &variables,
        default_phase_cwd(spec, "CONFIGURE"),
        "CONFIGURE",
        &spec.configure,
        log,
    )?;
    execute_portable_command_phase(
        spec,
        &paths,
        &base_env,
        &variables,
        default_phase_cwd(spec, "BUILD"),
        "BUILD",
        &spec.build,
        log,
    )?;
    execute_portable_command_phase(
        spec,
        &paths,
        &base_env,
        &variables,
        default_phase_cwd(spec, "INSTALL"),
        "INSTALL",
        &spec.install,
        log,
    )?;

    sync_host_staged_build_cell_caches(&build_cell, &job_root, log)?;

    apply_stage_entries(
        &paths.source_dir,
        &cache_build,
        &cache_prefix,
        &spec.stage_entries,
        log,
    )?;

    if spec.build_system == BuildSystem::Manual
        && spec.install.is_empty()
        && spec.stage_entries.is_empty()
    {
        return copy_declared_exports_from_candidates(
            depos_root,
            &[
                ExportCandidateRoot {
                    label: "build",
                    root: &cache_build,
                },
                ExportCandidateRoot {
                    label: "source",
                    root: &paths.source_dir,
                },
            ],
            store_root,
            spec,
            log,
        );
    }

    copy_declared_exports(depos_root, &cache_prefix, store_root, spec, log)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn execute_command_pipeline(
    depos_root: &Path,
    store_root: &Path,
    spec: &PackageSpec,
    _executable: &Path,
    source_root: &Path,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    validate_supported_command_pipeline(spec)?;
    if matches!(spec.build_root, BuildRoot::Oci(_)) {
        return linux_provider::execute_linux_provider_command_pipeline(
            depos_root,
            store_root,
            spec,
            source_root,
            log,
        );
    }
    let variant = variant_for_target(&spec.target_arch, spec.target_platform)?;
    let variant_root = depos_root.join("store").join(&variant);
    let dependency_specs = resolve_dependency_specs(depos_root, spec)?;

    let runtime_root_prefix = depos_root.join(".run").join("metalor-runtime");
    let runtime_package_root = runtime_root_prefix
        .join(&spec.name)
        .join(&spec.namespace)
        .join(&spec.version);
    if runtime_package_root.exists() {
        fs::remove_dir_all(&runtime_package_root)
            .with_context(|| format!("failed to remove {}", runtime_package_root.display()))?;
    }
    fs::create_dir_all(&runtime_package_root)
        .with_context(|| format!("failed to create {}", runtime_package_root.display()))?;

    let job_root = runtime_package_root.join("job");
    let cache_root = runtime_package_root.join("cache");
    let cache_build = cache_root.join("build");
    let cache_prefix = cache_root.join("prefix");
    let cache_tmp = cache_root.join("tmp");
    let build_cell = build_portable_command_cell(
        &runtime_package_root,
        &cache_build,
        &cache_prefix,
        &cache_tmp,
        &variant_root,
        source_root,
        &dependency_specs,
    )?;
    let job = prepare_portable_worker_job(&job_root, &build_cell).with_context(|| {
        format!(
            "failed to prepare portable worker job {}",
            job_root.display()
        )
    })?;
    let paths = portable_command_paths(&job.root)?;
    let variables = build_portable_command_variables(spec, &dependency_specs, &paths)?;
    let base_env = build_portable_command_environment(spec, &dependency_specs, &paths);

    execute_portable_command_phase(
        spec,
        &paths,
        &base_env,
        &variables,
        default_phase_cwd(spec, "CONFIGURE"),
        "CONFIGURE",
        &spec.configure,
        log,
    )?;
    execute_portable_command_phase(
        spec,
        &paths,
        &base_env,
        &variables,
        default_phase_cwd(spec, "BUILD"),
        "BUILD",
        &spec.build,
        log,
    )?;
    execute_portable_command_phase(
        spec,
        &paths,
        &base_env,
        &variables,
        default_phase_cwd(spec, "INSTALL"),
        "INSTALL",
        &spec.install,
        log,
    )?;

    sync_portable_worker_caches(&build_cell, &job)
        .context("failed to sync portable worker caches back to host")?;

    apply_stage_entries(
        &paths.source_dir,
        &cache_build,
        &cache_prefix,
        &spec.stage_entries,
        log,
    )?;

    if spec.build_system == BuildSystem::Manual
        && spec.install.is_empty()
        && spec.stage_entries.is_empty()
    {
        return copy_declared_exports_from_candidates(
            depos_root,
            &[
                ExportCandidateRoot {
                    label: "build",
                    root: &cache_build,
                },
                ExportCandidateRoot {
                    label: "source",
                    root: &paths.source_dir,
                },
            ],
            store_root,
            spec,
            log,
        );
    }

    copy_declared_exports(depos_root, &cache_prefix, store_root, spec, log)
}

#[cfg(target_os = "linux")]
fn prepare_command_container_root(
    depos_root: &Path,
    runtime_root_prefix: &Path,
    runtime_package_root: &Path,
    spec: &PackageSpec,
    log: &mut String,
) -> Result<PathBuf> {
    match &spec.build_root {
        BuildRoot::System | BuildRoot::Scratch => {
            let container_root = runtime_package_root.join("root");
            fs::create_dir_all(&container_root)
                .with_context(|| format!("failed to create {}", container_root.display()))?;
            Ok(container_root)
        }
        BuildRoot::Oci(reference) => {
            log.push_str(&format!("prepare oci rootfs {}\n", reference));
            let requested_arch = if spec.build_arch == host_arch() {
                None
            } else {
                Some(spec.build_arch.as_str())
            };
            prepare_oci_rootfs(
                reference,
                runtime_root_prefix,
                runtime_package_root,
                Some(&depos_root.join("oci-cache")),
                requested_arch,
            )
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
struct PortableCommandPaths {
    job_root: PathBuf,
    source_dir: PathBuf,
    build_dir: PathBuf,
    prefix_dir: PathBuf,
    deps_dir: PathBuf,
    tmp_dir: PathBuf,
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn portable_command_paths(job_root: &Path) -> Result<PortableCommandPaths> {
    let normalized_job_root = normalize_host_path(job_root);
    Ok(PortableCommandPaths {
        job_root: normalized_job_root.clone(),
        source_dir: portable_cell_host_path(&normalized_job_root, CELL_SOURCE_DIR)?,
        build_dir: portable_cell_host_path(&normalized_job_root, CELL_BUILD_DIR)?,
        prefix_dir: portable_cell_host_path(&normalized_job_root, CELL_PREFIX_DIR)?,
        deps_dir: portable_cell_host_path(&normalized_job_root, CELL_DEPS_DIR)?,
        tmp_dir: portable_cell_host_path(&normalized_job_root, CELL_TMP_DIR)?,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn portable_cell_host_path(job_root: &Path, cell_path: &str) -> Result<PathBuf> {
    if !cell_path.starts_with('/') {
        bail!("portable build-cell path must be absolute: {cell_path}");
    }
    let mut output = normalize_host_path(job_root);
    for component in cell_path.split('/') {
        match component {
            "" | "." => {}
            ".." => bail!("portable build-cell path must not contain '..': {cell_path}"),
            normal => output.push(normal),
        }
    }
    Ok(normalize_host_path(&output))
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn portable_dependency_root(job_root: &Path, dependency: &PackageSpec) -> Result<PathBuf> {
    portable_cell_host_path(
        job_root,
        &format!(
            "{}/{}",
            CELL_DEPS_DIR,
            display_path(&package_relative_store_root(dependency))
        ),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn build_portable_command_cell(
    runtime_package_root: &Path,
    cache_build: &Path,
    cache_prefix: &Path,
    cache_tmp: &Path,
    variant_root: &Path,
    source_root: &Path,
    dependency_specs: &[PackageSpec],
) -> Result<BuildCellSpec> {
    let imports = dependency_specs
        .iter()
        .map(|dependency| ImportSpec {
            source: HostPath::from(variant_root.join(package_relative_store_root(dependency))),
            destination: CellPath::from(format!(
                "{}/{}",
                CELL_DEPS_DIR,
                display_path(&package_relative_store_root(dependency))
            )),
        })
        .collect::<Vec<_>>();
    Ok(BuildCellSpec {
        root: HostPath::from(runtime_package_root.to_path_buf()),
        scratch: HostPath::from(runtime_package_root.join("scratch")),
        workspace_path: CellPath::from(CELL_SOURCE_DIR),
        workspace_seed: WorkspaceSeed::SnapshotDir(HostPath::from(source_root.to_path_buf())),
        imports,
        caches: vec![
            CacheSpec {
                source: HostPath::from(cache_build.to_path_buf()),
                destination: CellPath::from(CELL_BUILD_DIR),
            },
            CacheSpec {
                source: HostPath::from(cache_prefix.to_path_buf()),
                destination: CellPath::from(CELL_PREFIX_DIR),
            },
            CacheSpec {
                source: HostPath::from(cache_tmp.to_path_buf()),
                destination: CellPath::from(CELL_TMP_DIR),
            },
        ],
        exports: Vec::new(),
        command: CommandSpec {
            cwd: CellPath::from(CELL_SOURCE_DIR),
            executable: "portable-placeholder".to_string(),
            argv: Vec::new(),
        },
        env: Vec::new(),
        network: NetworkPolicy::Enabled,
        limits: Default::default(),
        cleanup: CleanupPolicy::Always,
    })
}

#[cfg(target_os = "linux")]
fn prepare_host_staged_build_cell(
    job_root: &Path,
    spec: &BuildCellSpec,
    log: &mut String,
) -> Result<()> {
    if job_root.exists() {
        fs::remove_dir_all(job_root).with_context(|| {
            format!(
                "failed to reset host-staged job root {}",
                job_root.display()
            )
        })?;
    }
    fs::create_dir_all(job_root).with_context(|| {
        format!(
            "failed to create host-staged job root {}",
            job_root.display()
        )
    })?;

    let workspace = portable_cell_host_path(job_root, spec.workspace_path.as_str())?;
    fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create {}", workspace.display()))?;
    match &spec.workspace_seed {
        WorkspaceSeed::Empty => {}
        WorkspaceSeed::SnapshotDir(source) => copy_host_staged_directory_contents(
            source.as_path(),
            &workspace,
            source.as_path(),
            log,
        )?,
        WorkspaceSeed::Archive(path) => bail!(
            "host-staged Linux SYSTEM fallback does not support archive workspace seeds: {}",
            path.as_path().display()
        ),
    }

    for import in &spec.imports {
        let destination = portable_cell_host_path(job_root, import.destination.as_str())?;
        copy_host_staged_path(
            import.source.as_path(),
            &destination,
            import.source.as_path(),
            log,
        )?;
    }
    for cache in &spec.caches {
        let destination = portable_cell_host_path(job_root, cache.destination.as_str())?;
        if cache.source.as_path().exists() {
            copy_host_staged_path(
                cache.source.as_path(),
                &destination,
                cache.source.as_path(),
                log,
            )?;
        } else {
            fs::create_dir_all(&destination)
                .with_context(|| format!("failed to create {}", destination.display()))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn sync_host_staged_build_cell_caches(
    spec: &BuildCellSpec,
    job_root: &Path,
    log: &mut String,
) -> Result<()> {
    for cache in &spec.caches {
        let source = portable_cell_host_path(job_root, cache.destination.as_str())?;
        copy_host_staged_path(&source, cache.source.as_path(), &source, log)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_host_staged_directory_contents(
    source: &Path,
    destination: &Path,
    source_root: &Path,
    log: &mut String,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    for entry in read_dir_sorted(source)? {
        copy_host_staged_path(
            &entry.path(),
            &destination.join(entry.file_name()),
            source_root,
            log,
        )?;
    }
    fs::set_permissions(destination, metadata.permissions())
        .with_context(|| format!("failed to set permissions on {}", destination.display()))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_host_staged_path(
    source: &Path,
    destination: &Path,
    source_root: &Path,
    log: &mut String,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    let file_type = metadata.file_type();
    remove_existing_path(destination)?;
    if file_type.is_symlink() {
        copy_symlink(source, destination, source_root, log)?;
        return Ok(());
    }
    if file_type.is_dir() {
        copy_host_staged_directory_contents(source, destination, source_root, log)?;
        return Ok(());
    }
    if !file_type.is_file() {
        bail!(
            "host-staged source '{}' is not a file, directory, or symlink",
            source.display()
        );
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    log.push_str(&format!(
        "copy file {} -> {}\n",
        source.display(),
        destination.display()
    ));
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    fs::set_permissions(destination, metadata.permissions())
        .with_context(|| format!("failed to set permissions on {}", destination.display()))?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn build_portable_command_variables(
    spec: &PackageSpec,
    dependency_specs: &[PackageSpec],
    paths: &PortableCommandPaths,
) -> Result<BTreeMap<String, String>> {
    let mut variables = BTreeMap::new();
    variables.insert("DEPO_PACKAGE_NAME".to_string(), spec.name.clone());
    variables.insert("DEPO_PACKAGE_NAMESPACE".to_string(), spec.namespace.clone());
    variables.insert("DEPO_PACKAGE_VERSION".to_string(), spec.version.clone());
    variables.insert(
        "DEPO_SOURCE_DIR".to_string(),
        display_path(&paths.source_dir),
    );
    variables.insert("DEPO_BUILD_DIR".to_string(), display_path(&paths.build_dir));
    variables.insert("DEPO_PREFIX".to_string(), display_path(&paths.prefix_dir));
    variables.insert("DEPO_DEPS_DIR".to_string(), display_path(&paths.deps_dir));
    variables.insert("DEPO_BUILD_ARCH".to_string(), spec.build_arch.clone());
    variables.insert("DEPO_TARGET_ARCH".to_string(), spec.target_arch.clone());
    variables.insert(
        "DEPO_TARGET_PLATFORM".to_string(),
        spec.target_platform.as_str().to_string(),
    );
    #[cfg(target_os = "linux")]
    {
        variables.insert(
            "DEPO_BUILD_TRIPLE".to_string(),
            linux_gnu_target_triple(&spec.build_arch).to_string(),
        );
        variables.insert(
            "DEPO_TARGET_TRIPLE".to_string(),
            linux_gnu_target_triple(&spec.target_arch).to_string(),
        );
    }
    for dependency in dependency_specs {
        let root = display_path(&portable_dependency_root(&paths.job_root, dependency)?);
        let namespaced_key = format!("dep:{}@{}", dependency.name, dependency.namespace);
        variables.insert(namespaced_key, root.clone());
        let simple_key = format!("dep:{}", dependency.name);
        match variables.get(&simple_key) {
            Some(existing) if existing != &root => bail!(
                "package '{}' has multiple dependencies named '{}' across namespaces; use ${{dep:{}@<namespace>}}",
                spec.package_id(),
                dependency.name,
                dependency.name
            ),
            Some(_) => {}
            None => {
                variables.insert(simple_key, root);
            }
        }
    }
    Ok(variables)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn build_portable_command_environment(
    spec: &PackageSpec,
    dependency_specs: &[PackageSpec],
    paths: &PortableCommandPaths,
) -> Vec<(String, String)> {
    let dependency_roots = dependency_specs
        .iter()
        .map(|dependency| portable_dependency_root(&paths.job_root, dependency))
        .collect::<Result<Vec<_>>>()
        .unwrap_or_default()
        .into_iter()
        .map(|path| display_path(&path))
        .collect::<Vec<_>>();
    let mut env = vec![
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
        ("HOME".to_string(), display_path(&paths.tmp_dir)),
        ("TMPDIR".to_string(), display_path(&paths.tmp_dir)),
        (
            "DEPO_SOURCE_DIR".to_string(),
            display_path(&paths.source_dir),
        ),
        ("DEPO_BUILD_DIR".to_string(), display_path(&paths.build_dir)),
        ("DEPO_PREFIX".to_string(), display_path(&paths.prefix_dir)),
        ("DEPO_DEPS_DIR".to_string(), display_path(&paths.deps_dir)),
        ("DEPO_BUILD_ARCH".to_string(), spec.build_arch.clone()),
        ("DEPO_TARGET_ARCH".to_string(), spec.target_arch.clone()),
        (
            "DEPO_TARGET_PLATFORM".to_string(),
            spec.target_platform.as_str().to_string(),
        ),
    ];
    #[cfg(target_os = "linux")]
    {
        env.extend([
            (
                "DEPO_BUILD_TRIPLE".to_string(),
                linux_gnu_target_triple(&spec.build_arch).to_string(),
            ),
            (
                "DEPO_TARGET_TRIPLE".to_string(),
                linux_gnu_target_triple(&spec.target_arch).to_string(),
            ),
        ]);
    }
    #[cfg(target_os = "windows")]
    {
        let tmp = display_path(&paths.tmp_dir);
        env.extend([
            ("USERPROFILE".to_string(), tmp.clone()),
            ("TMP".to_string(), tmp.clone()),
            ("TEMP".to_string(), tmp),
        ]);
        for key in [
            "COMSPEC",
            "ComSpec",
            "PATHEXT",
            "PathExt",
            "SYSTEMDRIVE",
            "SystemDrive",
            "SYSTEMROOT",
            "SystemRoot",
            "WINDIR",
            "Windir",
        ] {
            if let Ok(value) = std::env::var(key) {
                env.push((key.to_string(), value));
            }
        }
    }
    if spec.toolchain == ToolchainSource::System {
        env.extend(default_system_tool_environment());
    }
    if spec.build_system == BuildSystem::Cargo {
        env.push((
            "CARGO_HOME".to_string(),
            display_path(&paths.build_dir.join("cargo-home")),
        ));
    } else if let Some(cargo_home) = host_rust_toolchain_dir("CARGO_HOME", ".cargo") {
        env.push(("CARGO_HOME".to_string(), display_path(&cargo_home)));
    }
    if let Some(rustup_home) = host_rust_toolchain_dir("RUSTUP_HOME", ".rustup") {
        env.push(("RUSTUP_HOME".to_string(), display_path(&rustup_home)));
    }
    if !dependency_roots.is_empty() {
        env.push(("CMAKE_PREFIX_PATH".to_string(), dependency_roots.join(";")));
        let pkg_config_dirs = dependency_roots
            .iter()
            .flat_map(|root| {
                [
                    format!("{root}/lib/pkgconfig"),
                    format!("{root}/lib64/pkgconfig"),
                ]
            })
            .collect::<Vec<_>>();
        env.push((
            "PKG_CONFIG_LIBDIR".to_string(),
            join_host_path_list(&pkg_config_dirs),
        ));
        for (dependency, root) in dependency_specs.iter().zip(dependency_roots.iter()) {
            env.push((
                format!(
                    "DEPO_DEP_{}_{}_ROOT",
                    sanitize_env_fragment(&dependency.name),
                    sanitize_env_fragment(&dependency.namespace)
                ),
                root.clone(),
            ));
        }
    }
    let effective_system_libs = match spec.system_libs {
        PackageSystemLibs::Allow => PackageSystemLibs::Allow,
        PackageSystemLibs::Inherit | PackageSystemLibs::Never => PackageSystemLibs::Never,
    };
    if effective_system_libs != PackageSystemLibs::Allow {
        env.extend([
            ("PKG_CONFIG_DIR".to_string(), String::new()),
            ("PKG_CONFIG_PATH".to_string(), String::new()),
            ("CPATH".to_string(), String::new()),
            ("LIBRARY_PATH".to_string(), String::new()),
            ("C_INCLUDE_PATH".to_string(), String::new()),
            ("CPLUS_INCLUDE_PATH".to_string(), String::new()),
        ]);
    }
    env
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn default_system_tool_environment() -> Vec<(String, String)> {
    #[cfg(target_os = "linux")]
    {
        return vec![
            ("CC".to_string(), "cc".to_string()),
            ("CXX".to_string(), "c++".to_string()),
            ("AR".to_string(), "ar".to_string()),
            ("RANLIB".to_string(), "ranlib".to_string()),
            ("STRIP".to_string(), "strip".to_string()),
            ("CFLAGS".to_string(), String::new()),
            ("CXXFLAGS".to_string(), String::new()),
            ("LDFLAGS".to_string(), String::new()),
        ];
    }
    #[cfg(target_os = "macos")]
    {
        return vec![
            ("CC".to_string(), "clang".to_string()),
            ("CXX".to_string(), "clang++".to_string()),
            ("AR".to_string(), "ar".to_string()),
            ("RANLIB".to_string(), "ranlib".to_string()),
            ("STRIP".to_string(), "strip".to_string()),
            ("CFLAGS".to_string(), String::new()),
            ("CXXFLAGS".to_string(), String::new()),
            ("LDFLAGS".to_string(), String::new()),
        ];
    }
    #[cfg(target_os = "windows")]
    {
        let mut env = BTreeMap::new();
        for (key, value) in std::env::vars() {
            let upper = key.to_ascii_uppercase();
            let preserve = matches!(
                upper.as_str(),
                "AR" | "CC"
                    | "CL"
                    | "COMMANDPROMPTTYPE"
                    | "CXX"
                    | "DEVENVDIR"
                    | "EXTERNAL_INCLUDE"
                    | "INCLUDE"
                    | "LIB"
                    | "LIBPATH"
                    | "LINK"
                    | "MT"
                    | "PLATFORM"
                    | "PLATFORMTOOLSET"
                    | "PREFERREDTOOLARCHITECTURE"
                    | "RC"
                    | "UCRTVERSION"
                    | "UNIVERSALCRTSDKDIR"
                    | "VCIDEINSTALLDIR"
                    | "VCINSTALLDIR"
                    | "VCTOOLSINSTALLDIR"
                    | "VCTOOLSREDISTDIR"
                    | "VCTOOLSVERSION"
                    | "VISUALSTUDIOVERSION"
                    | "VSINSTALLDIR"
                    | "WINDOWSLIBPATH"
                    | "WINDOWSSDKBINPATH"
                    | "WINDOWSSDKDIR"
                    | "WINDOWSSDKLIBVERSION"
                    | "WINDOWSSDKVERSION"
                    | "WINDOWSSDKVERBINPATH"
                    | "_CL_"
                    | "_LINK_"
            ) || upper.starts_with("__VSCMD_")
                || upper.starts_with("EXTENSIONSDK")
                || upper.starts_with("FRAMEWORK")
                || upper.starts_with("UCRT")
                || upper.starts_with("UNIVERSALCRT")
                || upper.starts_with("VC")
                || upper.starts_with("VSCMD_")
                || upper.starts_with("VS")
                || upper.starts_with("WINDOWS");
            if preserve {
                env.insert(key, value);
            }
        }
        if env.contains_key("VSINSTALLDIR") {
            env.entry("CC".to_string())
                .or_insert_with(|| "cl".to_string());
            env.entry("CXX".to_string())
                .or_insert_with(|| "cl".to_string());
        }
        env.into_iter().collect()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn execute_portable_command_phase(
    spec: &PackageSpec,
    paths: &PortableCommandPaths,
    base_env: &[(String, String)],
    variables: &BTreeMap<String, String>,
    cwd: &str,
    phase_name: &str,
    commands: &[Vec<String>],
    log: &mut String,
) -> Result<()> {
    for command in commands {
        let is_shell_wrapper =
            command.len() == 4 && command[0] == "sh" && command[1] == "-eu" && command[2] == "-c";
        let argv = command
            .iter()
            .map(|arg| {
                let interpolated =
                    interpolate_braced_variables(arg, variables, "DepoFile command variable")?;
                if is_shell_wrapper {
                    Ok(interpolated)
                } else {
                    expand_trusted_direct_command_substitutions(&interpolated)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        if argv.is_empty() {
            bail!("{} command array must not be empty", phase_name);
        }
        let resolved_executable = resolve_phase_executable_for_spec(spec, cwd, &argv[0])?;
        let executable_path =
            translate_portable_phase_executable(&paths.job_root, &resolved_executable)?;
        let host_cwd = portable_cell_host_path(&paths.job_root, cwd)?;
        log.push_str(&format!(
            "run {} in {}: {}\n",
            phase_name,
            display_path(&host_cwd),
            argv.join(" ")
        ));
        let output = Command::new(&executable_path)
            .args(&argv[1..])
            .current_dir(&host_cwd)
            .env_clear()
            .envs(base_env.iter().cloned())
            .output()
            .with_context(|| {
                format!(
                    "failed to spawn {} command '{}' in {}",
                    phase_name,
                    executable_path.display(),
                    host_cwd.display()
                )
            })?;
        append_process_output(log, &output.stdout, &output.stderr);
        if !output.status.success() {
            bail!(
                "{} command failed with status {}",
                phase_name,
                output.status
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn translate_portable_phase_executable(job_root: &Path, executable: &str) -> Result<PathBuf> {
    let is_cell_path = [
        CELL_SOURCE_DIR,
        CELL_BUILD_DIR,
        CELL_PREFIX_DIR,
        CELL_DEPS_DIR,
        CELL_TMP_DIR,
    ]
    .iter()
    .any(|root| executable == *root || executable.starts_with(&format!("{root}/")));
    if is_cell_path {
        portable_cell_host_path(job_root, executable)
    } else {
        Ok(PathBuf::from(executable))
    }
}

#[cfg(target_os = "linux")]
fn validate_supported_command_pipeline(spec: &PackageSpec) -> Result<()> {
    if spec.target_platform != TargetPlatform::Host {
        bail!(
            "package '{}' target platform '{}' requires a native macOS BUILD_ROOT SYSTEM pipeline",
            spec.package_id(),
            spec.target_platform.as_str()
        );
    }
    match (&spec.build_root, &spec.toolchain) {
        (BuildRoot::System, ToolchainSource::System) => {
            if !spec.toolchain_inputs.is_empty() {
                bail!(
                    "package '{}' mixes BUILD_ROOT SYSTEM with TOOLCHAIN_INPUT entries, which is not supported",
                    spec.package_id()
                );
            }
        }
        (BuildRoot::System, ToolchainSource::Rootfs) => bail!(
            "package '{}' uses BUILD_ROOT SYSTEM with TOOLCHAIN ROOTFS, which is not supported",
            spec.package_id()
        ),
        (BuildRoot::Scratch, ToolchainSource::System) => {
            if spec.toolchain_inputs.is_empty() {
                bail!(
                    "package '{}' uses BUILD_ROOT SCRATCH but does not declare any TOOLCHAIN_INPUT entries",
                    spec.package_id()
                );
            }
        }
        (BuildRoot::Scratch, ToolchainSource::Rootfs) => bail!(
            "package '{}' uses BUILD_ROOT SCRATCH with TOOLCHAIN ROOTFS, which is not supported",
            spec.package_id(),
        ),
        (BuildRoot::Oci(reference), ToolchainSource::System) => bail!(
            "package '{}' uses BUILD_ROOT OCI {} without TOOLCHAIN ROOTFS, which is not supported",
            spec.package_id(),
            reference
        ),
        (BuildRoot::Oci(_), ToolchainSource::Rootfs) => {}
    }
    for toolchain_input in &spec.toolchain_inputs {
        validate_toolchain_input(toolchain_input)?;
    }
    let host = host_arch();
    if spec.build_arch == host && spec.target_arch == host {
        return Ok(());
    }
    if spec.build_arch != spec.target_arch {
        return match (&spec.build_root, &spec.toolchain) {
            (BuildRoot::Oci(_), ToolchainSource::Rootfs) => Ok(()),
            _ => bail!(
                "package '{}' requests BUILD_ARCH '{}' / TARGET_ARCH '{}', but only BUILD_ROOT OCI + TOOLCHAIN ROOTFS is restored for BUILD_ARCH != TARGET_ARCH in this pass",
                spec.package_id(),
                spec.build_arch,
                spec.target_arch
            ),
        };
    }
    match (&spec.build_root, &spec.toolchain) {
        (BuildRoot::Oci(_), ToolchainSource::Rootfs) => Ok(()),
        _ => bail!(
            "package '{}' requests BUILD_ARCH '{}' / TARGET_ARCH '{}', but only BUILD_ROOT OCI + TOOLCHAIN ROOTFS with BUILD_ARCH == TARGET_ARCH is restored for non-host-native command builds in this pass",
            spec.package_id(),
            spec.build_arch,
            spec.target_arch
        ),
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn validate_supported_command_pipeline(spec: &PackageSpec) -> Result<()> {
    if spec.target_platform != TargetPlatform::Host && !matches!(spec.build_root, BuildRoot::System)
    {
        bail!(
            "package '{}' target platform '{}' requires a native macOS BUILD_ROOT SYSTEM pipeline",
            spec.package_id(),
            spec.target_platform.as_str()
        );
    }
    if !spec.toolchain_inputs.is_empty() {
        bail!(
            "package '{}' uses TOOLCHAIN_INPUT, but TOOLCHAIN_INPUT is only supported on Linux BUILD_ROOT SCRATCH or BUILD_ROOT OCI paths",
            spec.package_id()
        );
    }
    match (&spec.build_root, &spec.toolchain) {
        (BuildRoot::System, ToolchainSource::System) => {}
        (BuildRoot::System, ToolchainSource::Rootfs) => bail!(
            "package '{}' uses TOOLCHAIN ROOTFS without BUILD_ROOT OCI; on macOS and Windows, Linux provider builds require BUILD_ROOT OCI <image>",
            spec.package_id()
        ),
        (BuildRoot::Scratch, _) => bail!(
            "package '{}' uses BUILD_ROOT SCRATCH, but BUILD_ROOT SCRATCH is only supported on Linux",
            spec.package_id()
        ),
        (BuildRoot::Oci(reference), ToolchainSource::System) => bail!(
            "package '{}' uses BUILD_ROOT OCI {} without TOOLCHAIN ROOTFS, which is not supported",
            spec.package_id(),
            reference
        ),
        (BuildRoot::Oci(_), ToolchainSource::Rootfs) => {}
    }
    let host = host_arch();
    if matches!(spec.build_root, BuildRoot::Oci(_)) {
        return Ok(());
    }
    if spec.build_arch != host || spec.target_arch != host {
        bail!(
            "package '{}' requests BUILD_ARCH '{}' / TARGET_ARCH '{}' without BUILD_ROOT OCI; on macOS and Windows, Linux-targeted advanced builds require BUILD_ROOT OCI <image>",
            spec.package_id(),
            spec.build_arch,
            spec.target_arch
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn prepare_command_emulator(
    container_root: &Path,
    host_arch: &str,
    spec: &PackageSpec,
    log: &mut String,
) -> Result<Option<String>> {
    match (&spec.build_root, &spec.toolchain) {
        (BuildRoot::Oci(_), ToolchainSource::Rootfs) => {
            let Some(guest_arch) = command_emulator_guest_arch(host_arch, spec)? else {
                return Ok(None);
            };
            let emulator = prepare_runtime_emulator(container_root, host_arch, &guest_arch)?;
            if let Some(emulator_path) = &emulator {
                log.push_str(&format!(
                    "stage emulator for {} on {}: {}\n",
                    guest_arch, host_arch, emulator_path
                ));
            }
            Ok(emulator)
        }
        _ => Ok(None),
    }
}

#[cfg(target_os = "linux")]
fn command_emulator_guest_arch(host_arch: &str, spec: &PackageSpec) -> Result<Option<String>> {
    let host_arch = normalize_arch_name(host_arch)?;
    let build_arch = normalize_arch_name(&spec.build_arch)?;
    if build_arch != host_arch {
        return Ok(Some(build_arch));
    }
    let target_arch = normalize_arch_name(&spec.target_arch)?;
    if target_arch != host_arch {
        return Ok(Some(target_arch));
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
fn build_command_variables(
    spec: &PackageSpec,
    dependency_specs: &[PackageSpec],
) -> Result<BTreeMap<String, String>> {
    let mut variables = BTreeMap::new();
    variables.insert("DEPO_PACKAGE_NAME".to_string(), spec.name.clone());
    variables.insert("DEPO_PACKAGE_NAMESPACE".to_string(), spec.namespace.clone());
    variables.insert("DEPO_PACKAGE_VERSION".to_string(), spec.version.clone());
    variables.insert("DEPO_SOURCE_DIR".to_string(), CELL_SOURCE_DIR.to_string());
    variables.insert("DEPO_BUILD_DIR".to_string(), CELL_BUILD_DIR.to_string());
    variables.insert("DEPO_PREFIX".to_string(), CELL_PREFIX_DIR.to_string());
    variables.insert("DEPO_DEPS_DIR".to_string(), CELL_DEPS_DIR.to_string());
    variables.insert("DEPO_BUILD_ARCH".to_string(), spec.build_arch.clone());
    variables.insert("DEPO_TARGET_ARCH".to_string(), spec.target_arch.clone());
    variables.insert(
        "DEPO_TARGET_PLATFORM".to_string(),
        spec.target_platform.as_str().to_string(),
    );
    variables.insert(
        "DEPO_BUILD_TRIPLE".to_string(),
        linux_gnu_target_triple(&spec.build_arch).to_string(),
    );
    variables.insert(
        "DEPO_TARGET_TRIPLE".to_string(),
        linux_gnu_target_triple(&spec.target_arch).to_string(),
    );
    for dependency in dependency_specs {
        let root = format!(
            "{}/{}",
            CELL_DEPS_DIR,
            display_path(&package_relative_store_root(dependency))
        );
        let namespaced_key = format!("dep:{}@{}", dependency.name, dependency.namespace);
        variables.insert(namespaced_key, root.clone());
        let simple_key = format!("dep:{}", dependency.name);
        match variables.get(&simple_key) {
            Some(existing) if existing != &root => bail!(
                "package '{}' has multiple dependencies named '{}' across namespaces; use ${{dep:{}@<namespace>}}",
                spec.package_id(),
                dependency.name,
                dependency.name
            ),
            Some(_) => {}
            None => {
                variables.insert(simple_key, root);
            }
        }
    }
    Ok(variables)
}

fn host_rust_toolchain_dir(env_var: &str, home_suffix: &str) -> Option<PathBuf> {
    let from_env = std::env::var_os(env_var).map(PathBuf::from);
    let from_home = host_home_dir().map(|home| home.join(home_suffix));
    match from_env.or(from_home) {
        Some(path) if path.is_absolute() && path.exists() => Some(path),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
const PROVIDER_CARGO_HOME_DIR: &str = "/work/build/provider-cargo-home";
#[cfg(target_os = "linux")]
const PROVIDER_RUSTUP_HOME_DIR: &str = "/work/build/provider-rustup-home";

#[cfg(target_os = "linux")]
fn linux_provider_mode_enabled() -> bool {
    matches!(
        std::env::var("DEPOS_INTERNAL_LINUX_PROVIDER").as_deref(),
        Ok("1")
    )
}

#[cfg(target_os = "linux")]
fn linux_provider_cache_root() -> Result<Option<PathBuf>> {
    let Some(value) = std::env::var_os("DEPOS_PROVIDER_CACHE_ROOT") else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        bail!(
            "DEPOS_PROVIDER_CACHE_ROOT must be an absolute path, got {}",
            path.display()
        );
    }
    Ok(Some(path))
}

#[cfg(target_os = "linux")]
fn linux_provider_cache_mounts() -> Result<Vec<BindMount>> {
    let Some(cache_root) = linux_provider_cache_root()? else {
        return Ok(Vec::new());
    };
    let cargo_home = cache_root.join("cargo-home");
    let rustup_home = cache_root.join("rustup-home");
    fs::create_dir_all(&cargo_home)
        .with_context(|| format!("failed to create {}", cargo_home.display()))?;
    fs::create_dir_all(&rustup_home)
        .with_context(|| format!("failed to create {}", rustup_home.display()))?;
    Ok(vec![
        BindMount {
            source: cargo_home,
            destination: PROVIDER_CARGO_HOME_DIR.to_string(),
            read_only: false,
        },
        BindMount {
            source: rustup_home,
            destination: PROVIDER_RUSTUP_HOME_DIR.to_string(),
            read_only: false,
        },
    ])
}

#[cfg(target_os = "linux")]
fn linux_provider_bootstrap_commands(spec: &PackageSpec) -> Result<Vec<Vec<String>>> {
    if !linux_provider_mode_enabled()
        || !matches!(
            (&spec.build_root, &spec.toolchain),
            (BuildRoot::Oci(_), ToolchainSource::Rootfs)
        )
    {
        return Ok(Vec::new());
    }
    let mut commands = Vec::new();
    if spec.build_arch != spec.target_arch {
        commands.extend(shell_phase(
            linux_provider_cross_toolchain_bootstrap_script(spec),
        ));
    }
    match spec.build_system {
        BuildSystem::Cmake => {
            commands.extend(shell_phase(linux_provider_cmake_bootstrap_script(spec)));
        }
        BuildSystem::Cargo => {
            commands.extend(shell_phase(linux_provider_cargo_bootstrap_script(spec)));
        }
        _ => {}
    }
    Ok(commands)
}

#[cfg(target_os = "linux")]
fn linux_provider_cross_toolchain_bootstrap_script(spec: &PackageSpec) -> String {
    let cross_package = debian_crossbuild_package(&spec.target_arch);
    let target_prefix = linux_gnu_toolchain_prefix(&spec.target_arch);
    format!(
        r#"
export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
  apt-get update
  apt-get install -y {cross_package}
elif ! command -v {target_prefix}-gcc >/dev/null 2>&1; then
  echo "cross-target Linux provider builds currently require a Debian or Ubuntu OCI base image, or a base image that already has {target_prefix}-gcc installed" >&2
  exit 1
fi
"#,
        cross_package = cross_package,
        target_prefix = target_prefix,
    )
}

#[cfg(target_os = "linux")]
fn linux_provider_cmake_bootstrap_script(spec: &PackageSpec) -> String {
    format!(
        r#"
export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
  apt-get update
  apt-get install -y build-essential cmake ninja-build pkg-config ca-certificates
else
  if ! command -v cmake >/dev/null 2>&1 || ! command -v ninja >/dev/null 2>&1; then
    echo "BUILD_SYSTEM CMAKE via the Linux provider currently requires a Debian or Ubuntu OCI base image, or a base image that already has cmake and ninja installed" >&2
    exit 1
  fi
  if [ "{build_arch}" = "{target_arch}" ]; then
    if ! command -v cc >/dev/null 2>&1 || ! command -v c++ >/dev/null 2>&1; then
      echo "native BUILD_SYSTEM CMAKE via the Linux provider currently requires a Debian or Ubuntu OCI base image, or a base image that already has a native C/C++ toolchain installed" >&2
      exit 1
    fi
  fi
fi
"#,
        build_arch = spec.build_arch,
        target_arch = spec.target_arch,
    )
}

#[cfg(target_os = "linux")]
fn linux_provider_cargo_bootstrap_script(spec: &PackageSpec) -> String {
    let build_triple = linux_gnu_target_triple(&spec.build_arch);
    let target_triple = linux_gnu_target_triple(&spec.target_arch);
    format!(
        r#"
export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
  apt-get update
  apt-get install -y build-essential clang cmake curl file git pkg-config ca-certificates
elif ! command -v cargo >/dev/null 2>&1; then
  echo "BUILD_SYSTEM CARGO via the Linux provider currently requires a Debian or Ubuntu OCI base image, or a base image that already has cargo installed" >&2
  exit 1
fi
export CARGO_HOME="{cargo_home}"
export RUSTUP_HOME="{rustup_home}"
export PATH="$CARGO_HOME/bin:$PATH"
if ! command -v rustup >/dev/null 2>&1; then
  curl --fail --location --silent --show-error https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo was unavailable after rustup bootstrap" >&2
  exit 1
fi
rustup target add {build_triple}
rustup target add {target_triple}
"#,
        cargo_home = PROVIDER_CARGO_HOME_DIR,
        rustup_home = PROVIDER_RUSTUP_HOME_DIR,
        build_triple = build_triple,
        target_triple = target_triple,
    )
}

#[cfg(target_os = "linux")]
fn build_command_environment(
    spec: &PackageSpec,
    dependency_specs: &[PackageSpec],
) -> Vec<(String, String)> {
    let dependency_roots = dependency_specs
        .iter()
        .map(|dependency| {
            format!(
                "{}/{}",
                CELL_DEPS_DIR,
                display_path(&package_relative_store_root(dependency))
            )
        })
        .collect::<Vec<_>>();
    let provider_oci_mode = linux_provider_mode_enabled()
        && matches!(
            (&spec.build_root, &spec.toolchain),
            (BuildRoot::Oci(_), ToolchainSource::Rootfs)
        );
    let path_value = match (&spec.build_root, &spec.toolchain) {
        (BuildRoot::System, ToolchainSource::System) => "/usr/bin:/bin".to_string(),
        (BuildRoot::Scratch, ToolchainSource::System) => {
            build_toolchain_path(&spec.toolchain_inputs)
        }
        (BuildRoot::Oci(_), ToolchainSource::Rootfs) => prepend_path_entries(
            &build_toolchain_path(&spec.toolchain_inputs),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        ),
        _ => "/usr/bin:/bin".to_string(),
    };
    let path_value = if provider_oci_mode {
        prepend_path_entries(&format!("{PROVIDER_CARGO_HOME_DIR}/bin"), &path_value)
    } else {
        path_value
    };
    let home_value = if provider_oci_mode {
        "/root".to_string()
    } else {
        CELL_TMP_DIR.to_string()
    };
    let mut env = vec![
        ("PATH".to_string(), path_value),
        ("HOME".to_string(), home_value),
        ("TMPDIR".to_string(), CELL_TMP_DIR.to_string()),
        ("DEPO_SOURCE_DIR".to_string(), CELL_SOURCE_DIR.to_string()),
        ("DEPO_BUILD_DIR".to_string(), CELL_BUILD_DIR.to_string()),
        ("DEPO_PREFIX".to_string(), CELL_PREFIX_DIR.to_string()),
        ("DEPO_DEPS_DIR".to_string(), CELL_DEPS_DIR.to_string()),
        ("DEPO_BUILD_ARCH".to_string(), spec.build_arch.clone()),
        ("DEPO_TARGET_ARCH".to_string(), spec.target_arch.clone()),
        (
            "DEPO_TARGET_PLATFORM".to_string(),
            spec.target_platform.as_str().to_string(),
        ),
        (
            "DEPO_BUILD_TRIPLE".to_string(),
            linux_gnu_target_triple(&spec.build_arch).to_string(),
        ),
        (
            "DEPO_TARGET_TRIPLE".to_string(),
            linux_gnu_target_triple(&spec.target_arch).to_string(),
        ),
    ];
    if spec.toolchain == ToolchainSource::System {
        env.extend([
            ("CC".to_string(), "cc".to_string()),
            ("CXX".to_string(), "c++".to_string()),
            ("AR".to_string(), "ar".to_string()),
            ("RANLIB".to_string(), "ranlib".to_string()),
            ("STRIP".to_string(), "strip".to_string()),
            ("CFLAGS".to_string(), String::new()),
            ("CXXFLAGS".to_string(), String::new()),
            ("LDFLAGS".to_string(), String::new()),
        ]);
    } else if matches!(
        (&spec.build_root, &spec.toolchain),
        (BuildRoot::Oci(_), ToolchainSource::Rootfs)
    ) && spec.build_arch != spec.target_arch
    {
        let target_prefix = linux_gnu_toolchain_prefix(&spec.target_arch);
        let host = host_arch();
        let qemu_guest_arch = if spec.build_arch != host {
            spec.build_arch.as_str()
        } else {
            spec.target_arch.as_str()
        };
        env.extend([
            ("CC".to_string(), format!("{target_prefix}-gcc")),
            ("CXX".to_string(), format!("{target_prefix}-g++")),
            ("AR".to_string(), format!("{target_prefix}-ar")),
            ("RANLIB".to_string(), format!("{target_prefix}-ranlib")),
            ("STRIP".to_string(), format!("{target_prefix}-strip")),
            ("CFLAGS".to_string(), String::new()),
            ("CXXFLAGS".to_string(), String::new()),
            ("LDFLAGS".to_string(), String::new()),
            (
                "QEMU_LD_PREFIX".to_string(),
                format!("/usr/{}", linux_gnu_toolchain_prefix(qemu_guest_arch)),
            ),
        ]);
        env.push(("PKG_CONFIG_ALLOW_CROSS".to_string(), "1".to_string()));
    }
    if matches!(
        (&spec.build_root, &spec.toolchain),
        (BuildRoot::System, ToolchainSource::System)
    ) {
        if spec.build_system == BuildSystem::Cargo {
            env.push((
                "CARGO_HOME".to_string(),
                format!("{CELL_BUILD_DIR}/cargo-home"),
            ));
        } else if host_rust_toolchain_dir("CARGO_HOME", ".cargo").is_some() {
            env.push((
                "CARGO_HOME".to_string(),
                "/.metalor-toolchain/cargo-home".to_string(),
            ));
        }
        if host_rust_toolchain_dir("RUSTUP_HOME", ".rustup").is_some() {
            env.push((
                "RUSTUP_HOME".to_string(),
                "/.metalor-toolchain/rustup-home".to_string(),
            ));
        }
    }
    if provider_oci_mode {
        env.push((
            "CARGO_HOME".to_string(),
            PROVIDER_CARGO_HOME_DIR.to_string(),
        ));
        env.push((
            "RUSTUP_HOME".to_string(),
            PROVIDER_RUSTUP_HOME_DIR.to_string(),
        ));
    }
    if spec.build_system == BuildSystem::Cargo && spec.build_arch != spec.target_arch {
        let target_triple = linux_gnu_target_triple(&spec.target_arch);
        let target_prefix = linux_gnu_toolchain_prefix(&spec.target_arch);
        let target_fragment = cargo_target_env_fragment(target_triple);
        let target_underscored = target_triple.replace('-', "_");
        let target_underscored_upper = target_underscored.to_ascii_uppercase();
        env.push(("CARGO_BUILD_TARGET".to_string(), target_triple.to_string()));
        env.push((
            format!("CARGO_TARGET_{target_fragment}_LINKER"),
            format!("{target_prefix}-gcc"),
        ));
        env.push((
            format!("CC_{target_underscored}"),
            format!("{target_prefix}-gcc"),
        ));
        env.push((
            format!("CC_{target_underscored_upper}"),
            format!("{target_prefix}-gcc"),
        ));
        env.push((
            format!("CXX_{target_underscored}"),
            format!("{target_prefix}-g++"),
        ));
        env.push((
            format!("CXX_{target_underscored_upper}"),
            format!("{target_prefix}-g++"),
        ));
        env.push((
            format!("AR_{target_underscored}"),
            format!("{target_prefix}-ar"),
        ));
        env.push((
            format!("AR_{target_underscored_upper}"),
            format!("{target_prefix}-ar"),
        ));
    }
    if !dependency_roots.is_empty() {
        env.push(("CMAKE_PREFIX_PATH".to_string(), dependency_roots.join(";")));
        let pkg_config_dirs = dependency_roots
            .iter()
            .flat_map(|root| {
                [
                    format!("{root}/lib/pkgconfig"),
                    format!("{root}/lib64/pkgconfig"),
                ]
            })
            .collect::<Vec<_>>();
        env.push(("PKG_CONFIG_LIBDIR".to_string(), pkg_config_dirs.join(":")));
        for dependency in dependency_specs {
            env.push((
                format!(
                    "DEPO_DEP_{}_{}_ROOT",
                    sanitize_env_fragment(&dependency.name),
                    sanitize_env_fragment(&dependency.namespace)
                ),
                format!(
                    "{}/{}",
                    CELL_DEPS_DIR,
                    display_path(&package_relative_store_root(dependency))
                ),
            ));
        }
    }
    let effective_system_libs = match spec.system_libs {
        PackageSystemLibs::Allow => PackageSystemLibs::Allow,
        PackageSystemLibs::Inherit | PackageSystemLibs::Never => PackageSystemLibs::Never,
    };
    if effective_system_libs != PackageSystemLibs::Allow {
        env.extend([
            ("PKG_CONFIG_DIR".to_string(), String::new()),
            ("PKG_CONFIG_PATH".to_string(), String::new()),
            ("CPATH".to_string(), String::new()),
            ("LIBRARY_PATH".to_string(), String::new()),
            ("C_INCLUDE_PATH".to_string(), String::new()),
            ("CPLUS_INCLUDE_PATH".to_string(), String::new()),
        ]);
    }
    env
}

#[cfg(target_os = "linux")]
fn build_command_mounts(
    spec: &PackageSpec,
    source_root: &Path,
    variant_root: &Path,
) -> Result<Vec<BindMount>> {
    let mut mounts = Vec::new();
    mounts.push(BindMount {
        source: source_root.to_path_buf(),
        destination: CELL_SOURCE_DIR.to_string(),
        read_only: false,
    });
    mounts.push(BindMount {
        source: variant_root.to_path_buf(),
        destination: CELL_DEPS_DIR.to_string(),
        read_only: true,
    });
    match spec.build_root {
        BuildRoot::System => {
            for (path, read_only) in [
                ("/usr", true),
                ("/bin", true),
                ("/lib", true),
                ("/lib64", true),
                ("/etc", true),
            ] {
                let host_path = Path::new(path);
                if host_path.exists() {
                    mounts.push(BindMount {
                        source: host_path.to_path_buf(),
                        destination: path.to_string(),
                        read_only,
                    });
                }
            }
            if let Some(cargo_home) = host_rust_toolchain_dir("CARGO_HOME", ".cargo") {
                mounts.push(BindMount {
                    source: cargo_home,
                    destination: "/.metalor-toolchain/cargo-home".to_string(),
                    read_only: true,
                });
            }
            if let Some(rustup_home) = host_rust_toolchain_dir("RUSTUP_HOME", ".rustup") {
                mounts.push(BindMount {
                    source: rustup_home,
                    destination: "/.metalor-toolchain/rustup-home".to_string(),
                    read_only: true,
                });
            }
        }
        BuildRoot::Scratch => {
            for (source, destination) in normalized_toolchain_inputs(&spec.toolchain_inputs) {
                mounts.push(BindMount {
                    destination,
                    source,
                    read_only: true,
                });
            }
        }
        BuildRoot::Oci(_) => {
            for (source, destination) in normalized_toolchain_inputs(&spec.toolchain_inputs) {
                mounts.push(BindMount {
                    destination,
                    source,
                    read_only: true,
                });
            }
            mounts.extend(linux_provider_cache_mounts()?);
        }
    }
    Ok(mounts)
}

#[cfg(target_os = "linux")]
struct PhaseExecutionContext<'a> {
    spec: &'a PackageSpec,
    executable: &'a Path,
    runtime_root_prefix: &'a Path,
    container_root: &'a Path,
    mounts: &'a [BindMount],
    emulator: Option<&'a str>,
    base_env: &'a [(String, String)],
    variables: &'a BTreeMap<String, String>,
    namespace_backend: LinuxNamespaceBackend,
}

#[cfg(target_os = "linux")]
fn execute_command_phase(
    context: &PhaseExecutionContext<'_>,
    cwd: &str,
    phase_name: &str,
    commands: &[Vec<String>],
    log: &mut String,
) -> Result<()> {
    for command in commands {
        let is_shell_wrapper =
            command.len() == 4 && command[0] == "sh" && command[1] == "-eu" && command[2] == "-c";
        let argv = command
            .iter()
            .map(|arg| {
                let interpolated = interpolate_braced_variables(
                    arg,
                    context.variables,
                    "DepoFile command variable",
                )?;
                if is_shell_wrapper {
                    Ok(interpolated)
                } else {
                    expand_trusted_direct_command_substitutions(&interpolated)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        if argv.is_empty() {
            bail!("{} command array must not be empty", phase_name);
        }
        let resolved_executable = resolve_phase_executable_for_spec(context.spec, cwd, &argv[0])?;
        log.push_str(&format!(
            "run {} in {}: {}\n",
            phase_name,
            cwd,
            argv.join(" ")
        ));
        let request = ContainerRunCommand {
            root: context.container_root.to_path_buf(),
            cwd: cwd.to_string(),
            mounts: context.mounts.to_vec(),
            env: context.base_env.to_vec(),
            emulator: context.emulator.map(ToOwned::to_owned),
            executable: resolved_executable,
            argv: argv[1..].to_vec(),
        };
        run_isolated_phase(
            context.executable,
            context.runtime_root_prefix,
            &request,
            context.namespace_backend,
            log,
        )?;
    }
    Ok(())
}

fn resolve_phase_executable_for_spec(
    spec: &PackageSpec,
    cwd: &str,
    executable: &str,
) -> Result<String> {
    let path = Path::new(executable);
    if path.is_absolute() || !executable.contains('/') {
        return resolve_phase_executable_for_backend(spec, executable);
    }

    let cwd_path = Path::new(cwd);
    if !cwd_path.is_absolute() {
        bail!("phase cwd must be absolute: {}", cwd);
    }

    let mut resolved = PathBuf::from(cwd_path);
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => resolved.push(part),
            Component::ParentDir => {
                bail!(
                    "relative phase executable must not contain '..': {}",
                    executable
                )
            }
            Component::RootDir | Component::Prefix(_) => unreachable!(),
        }
    }
    Ok(resolved.display().to_string())
}

#[cfg(target_os = "linux")]
fn resolve_phase_executable_for_backend(spec: &PackageSpec, executable: &str) -> Result<String> {
    match (&spec.build_root, &spec.toolchain) {
        (BuildRoot::Oci(_), ToolchainSource::Rootfs) => Ok(executable.to_string()),
        _ => resolve_runtime_executable(executable),
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn resolve_phase_executable_for_backend(_spec: &PackageSpec, executable: &str) -> Result<String> {
    Ok(executable.to_string())
}

fn expand_trusted_direct_command_substitutions(value: &str) -> Result<String> {
    let mut output = String::new();
    let mut remainder = value;
    while let Some(start) = remainder.find("$(") {
        output.push_str(&remainder[..start]);
        let after_start = &remainder[start + 2..];
        let end = after_start
            .find(')')
            .ok_or_else(|| anyhow!("unterminated direct command substitution in '{}'", value))?;
        let substitution = &after_start[..end];
        match substitution {
            "nproc" => output.push_str(
                &std::thread::available_parallelism()
                    .map(|count| count.get().to_string())
                    .unwrap_or_else(|_| "1".to_string()),
            ),
            other => {
                bail!(
                    "unsupported direct command substitution '$({})' in '{}'",
                    other,
                    value
                )
            }
        }
        remainder = &after_start[end + 1..];
    }
    output.push_str(remainder);
    Ok(output)
}

fn default_phase_cwd(spec: &PackageSpec, phase_name: &str) -> &'static str {
    match spec.build_system {
        BuildSystem::Autoconf => CELL_SOURCE_DIR,
        BuildSystem::Cargo => CELL_SOURCE_DIR,
        BuildSystem::Cmake | BuildSystem::Meson | BuildSystem::Manual => match phase_name {
            "CONFIGURE" => CELL_SOURCE_DIR,
            "BUILD" | "INSTALL" => CELL_BUILD_DIR,
            _ => CELL_SOURCE_DIR,
        },
    }
}

fn append_process_output(log: &mut String, stdout: &[u8], stderr: &[u8]) {
    if !stdout.is_empty() {
        log.push_str(&String::from_utf8_lossy(stdout));
        if !log.ends_with('\n') {
            log.push('\n');
        }
    }
    if !stderr.is_empty() {
        log.push_str(&String::from_utf8_lossy(stderr));
        if !log.ends_with('\n') {
            log.push('\n');
        }
    }
}

fn append_process_failure_output(message: &mut String, label: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let rendered = String::from_utf8_lossy(bytes);
    let rendered = rendered.trim_end_matches(['\r', '\n']);
    if rendered.is_empty() {
        return;
    }
    message.push_str(&format!("\n{label}:\n{rendered}"));
}

#[cfg(target_os = "linux")]
fn run_isolated_phase(
    executable: &Path,
    runtime_root_prefix: &Path,
    request: &ContainerRunCommand,
    namespace_backend: LinuxNamespaceBackend,
    log: &mut String,
) -> Result<()> {
    let mut command = build_unshare_reexec_command_with_backend(
        executable,
        "internal-run",
        runtime_root_prefix,
        request,
        namespace_backend,
    )?;
    let output = command
        .output()
        .context("failed to spawn isolated depos internal-run command")?;
    append_process_output(log, &output.stdout, &output.stderr);
    if !output.status.success() {
        bail!("isolated command failed with status {}", output.status);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn resolve_runtime_executable(executable: &str) -> Result<String> {
    if executable.starts_with('/') {
        return Ok(executable.to_string());
    }
    for prefix in ["/usr/bin", "/bin"] {
        let candidate = Path::new(prefix).join(executable);
        if candidate.exists() {
            return Ok(candidate.display().to_string());
        }
    }
    bail!("unable to resolve executable '{}'", executable);
}

#[cfg(target_os = "linux")]
fn validate_toolchain_input(toolchain_input: &str) -> Result<()> {
    let path = Path::new(toolchain_input);
    if !path.is_absolute() {
        bail!(
            "TOOLCHAIN_INPUT '{}' must be an absolute host path",
            toolchain_input
        );
    }
    if !path.exists() {
        bail!(
            "TOOLCHAIN_INPUT '{}' does not exist on the host",
            toolchain_input
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn normalized_toolchain_inputs(values: &[String]) -> Vec<(PathBuf, String)> {
    let mut output = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let destination = value.clone();
        if seen.insert(destination.clone()) {
            output.push((PathBuf::from(value), destination));
        }
    }
    output
}

#[cfg(target_os = "linux")]
fn build_toolchain_path(values: &[String]) -> String {
    let mut directories = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let path = Path::new(value);
        let candidate = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.to_path_buf())
        };
        let candidate_string = display_path(&candidate);
        if seen.insert(candidate_string.clone()) {
            directories.push(candidate_string);
        }
    }
    join_host_path_list(&directories)
}

#[cfg(target_os = "linux")]
fn prepend_path_entries(prefix: &str, fallback: &str) -> String {
    if prefix.is_empty() {
        fallback.to_string()
    } else {
        format!("{}{}{}", prefix, host_path_separator(), fallback)
    }
}

fn resolve_git_source(
    url: &str,
    reference: &str,
    submodules_recursive: bool,
    source_root: &Path,
    log: &mut String,
) -> Result<String> {
    log.push_str(&format!("resolve git {} {}\n", url, reference));
    let cloned = ensure_git_clone(url, source_root, log)?;
    if is_full_git_commit_reference(reference) {
        if let Some(commit) = resolve_git_commit_if_present(source_root, reference)? {
            if commit.eq_ignore_ascii_case(reference) {
                log.push_str(&format!(
                    "reuse exact git commit {} without fetch\n",
                    reference
                ));
                return Ok(commit);
            }
        }
    }
    if !cloned {
        run_command(
            log,
            Some(source_root),
            "git",
            ["fetch", "--tags", "--force", "--prune", "origin"],
            None,
        )?;
    }
    let checkout_reference = resolve_git_checkout_reference(source_root, reference)?;
    let desired_commit = resolve_git_reference_to_commit(source_root, &checkout_reference)?;
    if is_full_git_commit_reference(reference)
        && desired_commit.eq_ignore_ascii_case(reference)
        && git_submodules_ready(source_root, submodules_recursive)?
    {
        log.push_str(&format!(
            "resolved exact git commit {} without additional fetch work\n",
            reference
        ));
    }
    Ok(desired_commit)
}

fn git_source_command_argument(url: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        let path = Path::new(url);
        if path.is_absolute() {
            return display_path(path);
        }
    }
    url.to_string()
}

fn git_source_identity(url: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        let path = Path::new(url);
        if path.is_absolute() {
            return display_path(path);
        }
        return url.replace('\\', "/");
    }
    #[cfg(not(target_os = "windows"))]
    {
        url.to_string()
    }
}

fn ensure_git_clone(url: &str, source_root: &Path, log: &mut String) -> Result<bool> {
    if source_root.exists() {
        if is_git_worktree(source_root) && git_remote_matches_url(source_root, url)? {
            return Ok(false);
        }
        fs::remove_dir_all(source_root)
            .with_context(|| format!("failed to remove {}", source_root.display()))?;
    }
    if let Some(parent) = source_root.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let clone_source = git_source_command_argument(url);
    run_command(
        log,
        None,
        "git",
        vec!["clone".to_string(), clone_source, display_path(source_root)],
        Some(source_root),
    )?;
    Ok(true)
}

fn prepare_git_source(
    source_root: &Path,
    desired_commit: &str,
    submodules_recursive: bool,
    log: &mut String,
) -> Result<()> {
    let current_commit = resolve_checked_out_git_commit(source_root)?;
    if current_commit == desired_commit
        && git_worktree_clean(source_root)?
        && git_submodules_ready(source_root, submodules_recursive)?
    {
        log.push_str(&format!("reuse clean git checkout {}\n", desired_commit));
        return Ok(());
    }
    log.push_str(&format!("checkout git commit {}\n", desired_commit));
    run_command(
        log,
        Some(source_root),
        "git",
        ["checkout", "--force", desired_commit],
        None,
    )?;
    run_command(log, Some(source_root), "git", ["clean", "-fdx"], None)?;
    if submodules_recursive {
        run_command(
            log,
            Some(source_root),
            "git",
            ["submodule", "update", "--init", "--recursive"],
            None,
        )?;
    }
    Ok(())
}

fn resolve_git_checkout_reference(source_root: &Path, reference: &str) -> Result<String> {
    if reference == "HEAD" {
        if let Some(remote_head) = git_output(
            source_root,
            [
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
        )? {
            return Ok(remote_head.trim().to_string());
        }
        return Ok("FETCH_HEAD".to_string());
    }

    let remote_reference = format!("refs/remotes/origin/{reference}");
    if git_status_success(
        source_root,
        ["show-ref", "--verify", "--quiet", &remote_reference],
    )? {
        return Ok(remote_reference);
    }
    Ok(reference.to_string())
}

fn resolve_checked_out_git_commit(source_root: &Path) -> Result<String> {
    let commit = git_output(source_root, ["rev-parse", "HEAD"])?.with_context(|| {
        format!(
            "unable to resolve checked-out commit under {}",
            source_root.display()
        )
    })?;
    Ok(commit.trim().to_string())
}

fn resolve_git_commit_if_present(source_root: &Path, reference: &str) -> Result<Option<String>> {
    let verify_target = format!("{reference}^{{commit}}");
    Ok(git_output(
        source_root,
        ["rev-parse", "--verify", "--quiet", verify_target.as_str()],
    )?
    .map(|value| value.trim().to_string()))
}

fn resolve_git_reference_to_commit(source_root: &Path, reference: &str) -> Result<String> {
    let commit = git_output(source_root, ["rev-parse", reference])?.with_context(|| {
        format!(
            "unable to resolve git reference '{}' under {}",
            reference,
            source_root.display()
        )
    })?;
    Ok(commit.trim().to_string())
}

fn is_full_git_commit_reference(reference: &str) -> bool {
    reference.len() == 40
        && reference
            .chars()
            .all(|character| character.is_ascii_hexdigit())
}

fn is_git_worktree(source_root: &Path) -> bool {
    source_root.join(".git").exists()
}

fn git_remote_matches_url(source_root: &Path, url: &str) -> Result<bool> {
    Ok(
        git_output(source_root, ["config", "--get", "remote.origin.url"])?
            .map(|value| git_source_identity(value.trim()) == git_source_identity(url))
            .unwrap_or(false),
    )
}

fn git_worktree_clean(source_root: &Path) -> Result<bool> {
    Ok(git_output(
        source_root,
        ["status", "--porcelain", "--untracked-files=all"],
    )?
    .map(|value| value.trim().is_empty())
    .unwrap_or(false))
}

fn git_submodules_ready(source_root: &Path, submodules_recursive: bool) -> Result<bool> {
    if !submodules_recursive {
        return Ok(true);
    }
    let Some(status) = git_output(source_root, ["submodule", "status", "--recursive"])? else {
        return Ok(false);
    };
    Ok(status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| line.starts_with(' ')))
}

fn git_output<const N: usize>(current_dir: &Path, argv: [&str; N]) -> Result<Option<String>> {
    let executable_path = resolve_command_path("git");
    let output = Command::new(&executable_path)
        .args(argv)
        .current_dir(normalize_host_path(current_dir))
        .output()
        .with_context(|| format!("failed to spawn {}", executable_path.display()))?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8(output.stdout).context("git output was not valid utf-8")?,
        ));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    bail!(
        "git {} failed with status {}",
        argv.join(" "),
        output.status
    );
}

fn git_status_success<const N: usize>(current_dir: &Path, argv: [&str; N]) -> Result<bool> {
    let executable_path = resolve_command_path("git");
    let status = Command::new(&executable_path)
        .args(argv)
        .current_dir(normalize_host_path(current_dir))
        .status()
        .with_context(|| format!("failed to spawn {}", executable_path.display()))?;
    Ok(status.success())
}

fn resolve_url_source(
    url: &str,
    sha256: Option<&str>,
    archive_path: &Path,
    log: &mut String,
) -> Result<String> {
    log.push_str(&format!("resolve url {}\n", url));
    if let Some(parent) = archive_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(expected) = sha256 {
        if archive_path.exists() {
            let actual = hash_file_sha256(archive_path)?;
            if actual == expected {
                log.push_str(&format!(
                    "reuse cached url archive {} with sha256 {}\n",
                    archive_path.display(),
                    expected
                ));
                validate_archive_entries(archive_path)?;
                return Ok(actual);
            }
        }
        return download_and_publish_url_archive(url, archive_path, Some(expected), log);
    }

    if let Some(local_path) = file_url_path(url) {
        let expected = hash_file_sha256(&local_path)?;
        if archive_path.exists() && hash_file_sha256(archive_path)? == expected {
            log.push_str(&format!(
                "reuse cached file url archive {} from {}\n",
                archive_path.display(),
                local_path.display()
            ));
            validate_archive_entries(archive_path)?;
            return Ok(expected);
        }
        return download_and_publish_url_archive(url, archive_path, Some(&expected), log);
    }

    download_and_publish_url_archive(url, archive_path, None, log)
}

fn download_and_publish_url_archive(
    url: &str,
    archive_path: &Path,
    expected_sha256: Option<&str>,
    log: &mut String,
) -> Result<String> {
    let temporary = create_archive_download_path(archive_path)?;
    let result = (|| {
        download_url_archive(url, &temporary, log)?;
        let actual = hash_file_sha256(&temporary)?;
        if let Some(expected) = expected_sha256 {
            if actual != expected {
                bail!(
                    "downloaded archive '{}' sha256 mismatch: expected {}, got {}",
                    archive_path.display(),
                    expected,
                    actual
                );
            }
        }
        validate_archive_entries(&temporary)?;
        fs::File::open(&temporary)
            .with_context(|| format!("failed to open downloaded archive {}", temporary.display()))?
            .sync_all()
            .with_context(|| {
                format!("failed to sync downloaded archive {}", temporary.display())
            })?;
        publish_downloaded_archive(&temporary, archive_path)?;
        Ok(actual)
    })();
    if result.is_err() {
        let _ = remove_existing_path(&temporary);
    }
    result
}

fn create_archive_download_path(archive_path: &Path) -> Result<PathBuf> {
    let parent = archive_path
        .parent()
        .with_context(|| format!("archive path '{}' has no parent", archive_path.display()))?;
    let name = archive_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("archive"))
        .to_string_lossy();
    for attempt in 0..1_024_u32 {
        let candidate = parent.join(format!(
            ".{name}.depos-download-{}-{attempt}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create archive download file {}",
                        candidate.display()
                    )
                });
            }
        }
    }
    bail!(
        "unable to allocate an archive download file beside '{}'",
        archive_path.display()
    )
}

fn publish_downloaded_archive(temporary: &Path, archive_path: &Path) -> Result<()> {
    let previous = match fs::symlink_metadata(archive_path) {
        Ok(metadata) => {
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                bail!(
                    "archive cache path '{}' must not be a directory",
                    archive_path.display()
                );
            }
            Some(unused_zip_backup_path(archive_path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect archive cache path {}",
                    archive_path.display()
                )
            });
        }
    };
    let parent = archive_path
        .parent()
        .with_context(|| format!("archive path '{}' has no parent", archive_path.display()))?;
    if let Some(previous) = &previous {
        fs::rename(archive_path, previous).with_context(|| {
            format!(
                "failed to preserve prior archive '{}' as '{}'",
                archive_path.display(),
                previous.display()
            )
        })?;
        sync_directory(parent)?;
    }
    if let Err(install_error) = fs::rename(temporary, archive_path) {
        if let Some(previous) = &previous {
            if let Err(rollback_error) = fs::rename(previous, archive_path) {
                bail!(
                    "failed to publish archive '{}' to '{}': {}; rollback from '{}' also failed: {}",
                    temporary.display(),
                    archive_path.display(),
                    install_error,
                    previous.display(),
                    rollback_error
                );
            }
            sync_directory(parent)?;
        }
        return Err(install_error).with_context(|| {
            format!(
                "failed to publish archive '{}' to '{}'",
                temporary.display(),
                archive_path.display()
            )
        });
    }
    sync_directory(parent)?;
    if let Some(previous) = previous {
        remove_existing_path(&previous)
            .with_context(|| format!("failed to remove prior archive {}", previous.display()))?;
        sync_directory(parent)?;
    }
    Ok(())
}

fn prepare_url_source(archive_path: &Path, source_root: &Path, log: &mut String) -> Result<()> {
    let archive_kind = archive_kind(archive_path)?;
    if archive_kind == ArchiveKind::Zip {
        let (mut archive, entries) =
            open_validated_zip_archive(archive_path, DEPENDENCY_ZIP_LIMITS)?;
        let staging_root = create_zip_staging_root(source_root)?;
        log.push_str(&format!(
            "extract url archive {} -> {}\n",
            archive_path.display(),
            staging_root.display()
        ));
        if let Err(error) = extract_zip_archive(
            archive_path,
            &staging_root,
            &entries,
            &mut archive,
            DEPENDENCY_ZIP_LIMITS,
        ) {
            let _ = remove_existing_path(&staging_root);
            return Err(error);
        }
        if let Err(error) = publish_zip_source_tree(&staging_root, source_root) {
            let _ = remove_existing_path(&staging_root);
            return Err(error);
        }
        return Ok(());
    }

    if source_root.exists() {
        fs::remove_dir_all(source_root)
            .with_context(|| format!("failed to remove {}", source_root.display()))?;
    }
    fs::create_dir_all(source_root)
        .with_context(|| format!("failed to create {}", source_root.display()))?;
    log.push_str(&format!(
        "extract url archive {} -> {}\n",
        archive_path.display(),
        source_root.display()
    ));
    run_command(
        log,
        None,
        "tar",
        [
            "--no-same-owner",
            "-xf",
            archive_path
                .to_str()
                .ok_or_else(|| anyhow!("non-utf8 archive path {}", archive_path.display()))?,
            "-C",
            source_root
                .to_str()
                .ok_or_else(|| anyhow!("non-utf8 source path {}", source_root.display()))?,
            "--strip-components=1",
        ],
        None,
    )?;
    Ok(())
}

fn download_url_archive(url: &str, archive_path: &Path, log: &mut String) -> Result<()> {
    download_url_archive_with_limits(url, archive_path, log, DEPENDENCY_DOWNLOAD_LIMITS)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UrlDownloadLimits {
    bytes: u64,
    transfer_timeout: std::time::Duration,
}

fn download_url_archive_with_limits(
    url: &str,
    archive_path: &Path,
    log: &mut String,
    limits: UrlDownloadLimits,
) -> Result<()> {
    if limits.bytes == 0 || limits.transfer_timeout.is_zero() {
        bail!("URL download limits must be greater than zero");
    }
    let archive = archive_path
        .to_str()
        .ok_or_else(|| anyhow!("non-utf8 archive path {}", archive_path.display()))?;
    let max_bytes = limits.bytes.to_string();
    let max_time = format!("{:.3}", limits.transfer_timeout.as_secs_f64());
    run_command(
        log,
        None,
        "curl",
        [
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--max-time",
            &max_time,
            "--max-filesize",
            &max_bytes,
            "--output",
            archive,
            url,
        ],
        None,
    )
    .with_context(|| {
        format!(
            "URL download failed within its {} byte and {:.3} second transfer limits",
            limits.bytes,
            limits.transfer_timeout.as_secs_f64()
        )
    })?;
    let downloaded_bytes = fs::metadata(archive_path)
        .with_context(|| {
            format!(
                "failed to inspect downloaded archive {}",
                archive_path.display()
            )
        })?
        .len();
    if downloaded_bytes > limits.bytes {
        bail!(
            "downloaded archive '{}' exceeds the {} byte transfer limit",
            archive_path.display(),
            limits.bytes
        );
    }
    Ok(())
}

fn file_url_path(url: &str) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://")?;
    #[cfg(target_os = "windows")]
    {
        let raw = if raw.len() >= 4
            && raw.starts_with('/')
            && raw.as_bytes()[2] == b':'
            && raw.as_bytes()[1].is_ascii_alphabetic()
        {
            &raw[1..]
        } else {
            raw
        };
        return Some(PathBuf::from(raw));
    }
    #[cfg(not(target_os = "windows"))]
    {
        Some(PathBuf::from(raw))
    }
}

fn validate_archive_entries(archive_path: &Path) -> Result<()> {
    match archive_kind(archive_path)? {
        ArchiveKind::Tar => validate_tar_archive_entries(archive_path),
        ArchiveKind::Zip => validate_zip_archive(archive_path).map(|_| ()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveKind {
    Tar,
    Zip,
}

fn archive_kind(archive_path: &Path) -> Result<ArchiveKind> {
    if archive_path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        return Ok(ArchiveKind::Zip);
    }

    let mut signature = [0_u8; 4];
    let mut archive = fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
    let read = archive
        .read(&mut signature)
        .with_context(|| format!("failed to inspect archive {}", archive_path.display()))?;
    if read == signature.len()
        && matches!(
            signature,
            [b'P', b'K', 3, 4] | [b'P', b'K', 5, 6] | [b'P', b'K', 7, 8]
        )
    {
        Ok(ArchiveKind::Zip)
    } else {
        Ok(ArchiveKind::Tar)
    }
}

fn validate_tar_archive_entries(archive_path: &Path) -> Result<()> {
    let executable_path = resolve_command_path("tar");
    let output = Command::new(&executable_path)
        .args([
            "-tf",
            archive_path
                .to_str()
                .ok_or_else(|| anyhow!("non-utf8 archive path {}", archive_path.display()))?,
        ])
        .output()
        .with_context(|| format!("failed to spawn {}", executable_path.display()))?;
    if !output.status.success() {
        bail!(
            "tar -tf {} failed with status {}",
            archive_path.display(),
            output.status
        );
    }
    let listing =
        String::from_utf8(output.stdout).context("archive member list was not valid utf-8")?;
    for entry in listing.lines().filter(|line| !line.is_empty()) {
        ensure_archive_member_path_safe(Path::new(entry), archive_path)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ZipEntryKind {
    Directory,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ZipExtractionLimits {
    archive_bytes: u64,
    entries: usize,
    member_bytes: u64,
    total_bytes: u64,
    compression_ratio: u64,
    path_bytes: usize,
}

#[derive(Clone, Debug)]
struct ValidatedZipEntry {
    index: usize,
    path: PathBuf,
    kind: ZipEntryKind,
    unix_mode: Option<u32>,
    declared_size: u64,
}

#[derive(Clone, Debug)]
struct ZipPathNode {
    path: PathBuf,
    kind: ZipEntryKind,
    explicit: bool,
}

fn validate_zip_archive(archive_path: &Path) -> Result<Vec<ValidatedZipEntry>> {
    validate_zip_archive_with_limits(archive_path, DEPENDENCY_ZIP_LIMITS)
}

fn validate_zip_archive_with_limits(
    archive_path: &Path,
    limits: ZipExtractionLimits,
) -> Result<Vec<ValidatedZipEntry>> {
    open_validated_zip_archive(archive_path, limits).map(|(_, entries)| entries)
}

fn open_validated_zip_archive(
    archive_path: &Path,
    limits: ZipExtractionLimits,
) -> Result<(zip::ZipArchive<fs::File>, Vec<ValidatedZipEntry>)> {
    validate_zip_limits(limits)?;
    let path_metadata = fs::symlink_metadata(archive_path)
        .with_context(|| format!("failed to inspect ZIP archive {}", archive_path.display()))?;
    if !path_metadata.file_type().is_file() {
        bail!(
            "ZIP archive '{}' must be a non-symlink regular file",
            archive_path.display()
        );
    }
    if path_metadata.len() > limits.archive_bytes {
        bail!(
            "ZIP archive '{}' exceeds the {} byte archive limit",
            archive_path.display(),
            limits.archive_bytes
        );
    }
    let archive_file = fs::File::open(archive_path)
        .with_context(|| format!("failed to open ZIP archive {}", archive_path.display()))?;
    let opened_metadata = archive_file.metadata().with_context(|| {
        format!(
            "failed to inspect open ZIP archive {}",
            archive_path.display()
        )
    })?;
    if !opened_metadata.is_file() || opened_metadata.len() != path_metadata.len() {
        bail!(
            "ZIP archive '{}' changed while it was opened",
            archive_path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened_metadata.dev() != path_metadata.dev()
            || opened_metadata.ino() != path_metadata.ino()
        {
            bail!(
                "ZIP archive '{}' changed while it was opened",
                archive_path.display()
            );
        }
    }
    let mut archive = zip::ZipArchive::new(archive_file)
        .with_context(|| format!("failed to read ZIP archive {}", archive_path.display()))?;
    if archive.len() > limits.entries {
        bail!(
            "ZIP archive '{}' contains more than {} entries",
            archive_path.display(),
            limits.entries
        );
    }
    let mut entries = Vec::with_capacity(archive.len());
    let mut declared_total = 0_u64;

    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).with_context(|| {
            format!(
                "failed to read ZIP archive '{}' entry {index}",
                archive_path.display()
            )
        })?;
        let name = entry.name();
        let context = format!("ZIP archive '{}' entry '{}'", archive_path.display(), name);
        let kind = validate_zip_entry_kind(&entry, &context)?;
        let path = zip_entry_path(name, &context, limits.path_bytes)?;
        if kind == ZipEntryKind::File {
            if entry.size() > limits.member_bytes {
                bail!(
                    "{context} expands beyond the {} byte member limit",
                    limits.member_bytes
                );
            }
            declared_total = declared_total
                .checked_add(entry.size())
                .with_context(|| format!("{context} overflows the declared ZIP size total"))?;
            if declared_total > limits.total_bytes {
                bail!(
                    "ZIP archive '{}' expands beyond the {} byte total limit",
                    archive_path.display(),
                    limits.total_bytes
                );
            }
            if entry.size() > 0
                && (entry.compressed_size() == 0
                    || entry.size()
                        > entry
                            .compressed_size()
                            .saturating_mul(limits.compression_ratio))
            {
                bail!(
                    "{context} exceeds the {}:1 compression-ratio limit",
                    limits.compression_ratio
                );
            }
        }
        entries.push(ValidatedZipEntry {
            index,
            path,
            kind,
            unix_mode: entry.unix_mode(),
            declared_size: entry.size(),
        });
    }

    let strip_root = zip_common_root(&entries);
    let mut paths = BTreeMap::<String, ZipPathNode>::new();
    let mut validated = Vec::with_capacity(entries.len());
    for mut entry in entries {
        if strip_root {
            entry.path = entry.path.components().skip(1).collect();
            if entry.path.as_os_str().is_empty() {
                continue;
            }
        }
        let context = format!(
            "ZIP archive '{}' output '{}'",
            archive_path.display(),
            entry.path.display()
        );
        register_zip_path(&mut paths, &entry.path, entry.kind, &context)?;
        validated.push(entry);
    }
    Ok((archive, validated))
}

fn validate_zip_limits(limits: ZipExtractionLimits) -> Result<()> {
    if limits.archive_bytes == 0
        || limits.entries == 0
        || limits.member_bytes == 0
        || limits.total_bytes == 0
        || limits.compression_ratio == 0
        || limits.path_bytes == 0
    {
        bail!("ZIP extraction limits must all be positive");
    }
    Ok(())
}

fn zip_common_root(entries: &[ValidatedZipEntry]) -> bool {
    let Some(root) = entries
        .first()
        .and_then(|entry| entry.path.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .map(normalized_zip_text)
    else {
        return false;
    };
    entries
        .iter()
        .any(|entry| entry.path.components().count() > 1)
        && entries.iter().all(|entry| {
            entry.path.components().next().is_some_and(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|component| normalized_zip_text(component) == root)
            }) && (entry.path.components().count() > 1 || entry.kind == ZipEntryKind::Directory)
        })
}

fn validate_zip_entry_kind(
    entry: &zip::read::ZipFile<'_, fs::File>,
    context: &str,
) -> Result<ZipEntryKind> {
    const FILE_TYPE_MASK: u32 = 0o170000;
    const DIRECTORY: u32 = 0o040000;
    const REGULAR_FILE: u32 = 0o100000;
    const SYMLINK: u32 = 0o120000;

    if !matches!(
        entry.compression(),
        zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
    ) {
        bail!(
            "{context} uses unsupported ZIP compression method {:?}; only Stored and Deflated are supported",
            entry.compression()
        );
    }
    if entry.encrypted() {
        bail!("{context} is encrypted; encrypted ZIP entries are not supported");
    }
    let kind = if entry.is_dir() {
        ZipEntryKind::Directory
    } else {
        ZipEntryKind::File
    };
    if entry.is_symlink() {
        bail!("{context} is a symbolic link; ZIP links are not extracted");
    }
    if let Some(mode) = entry.unix_mode() {
        match (mode & FILE_TYPE_MASK, kind) {
            (0, _) | (DIRECTORY, ZipEntryKind::Directory) | (REGULAR_FILE, ZipEntryKind::File) => {}
            (SYMLINK, _) => bail!("{context} is a symbolic link; ZIP links are not extracted"),
            (file_type, _) => bail!(
                "{context} has unsupported special file type {file_type:#o}; only regular files and directories are allowed"
            ),
        }
    }
    Ok(kind)
}

fn zip_entry_path(name: &str, context: &str, max_path_bytes: usize) -> Result<PathBuf> {
    if name.is_empty() || name.contains('\0') {
        bail!("{context} has an empty or NUL-containing path");
    }
    if name.len() > max_path_bytes {
        bail!("{context} exceeds the {max_path_bytes} byte path limit");
    }
    if name.chars().any(char::is_control) {
        bail!("{context} contains a control character");
    }
    if name.starts_with('/') {
        bail!("{context} must be relative");
    }
    if name.contains('\\') {
        bail!("{context} contains a backslash path separator");
    }

    let path = name.strip_suffix('/').unwrap_or(name);
    let components = path.split('/').collect::<Vec<_>>();
    if components.iter().any(|component| component.is_empty()) {
        bail!("{context} contains an empty path component");
    }
    if components
        .iter()
        .any(|component| matches!(*component, "." | ".."))
    {
        bail!("{context} contains '.' or '..' path traversal");
    }
    if components.iter().any(|component| component.contains(':')) {
        bail!("{context} contains a Windows drive or alternate-stream path");
    }
    if components
        .iter()
        .any(|component| windows_reserved_path_component(component))
    {
        bail!("{context} contains a reserved Windows device path");
    }
    if components
        .iter()
        .any(|component| component.ends_with([' ', '.']))
    {
        bail!("{context} contains a path component with a trailing space or dot");
    }

    Ok(components.iter().collect())
}

fn windows_reserved_path_component(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.len() == 4
            && (stem.as_bytes().starts_with(b"COM") || stem.as_bytes().starts_with(b"LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9')
}

fn zip_path_key(path: &Path) -> String {
    path.components()
        .map(|component| {
            normalized_zip_text(
                component
                    .as_os_str()
                    .to_str()
                    .expect("validated ZIP paths are UTF-8"),
            )
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn normalized_zip_text(value: &str) -> String {
    value.nfc().flat_map(char::to_lowercase).nfc().collect()
}

fn register_zip_path(
    paths: &mut BTreeMap<String, ZipPathNode>,
    path: &Path,
    kind: ZipEntryKind,
    context: &str,
) -> Result<()> {
    let components = path.components().collect::<Vec<_>>();
    let mut ancestor = PathBuf::new();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        ancestor.push(component.as_os_str());
        let key = zip_path_key(&ancestor);
        match paths.get(&key) {
            Some(node) if node.kind == ZipEntryKind::File => bail!(
                "{context} collides with file '{}' used as a parent path",
                node.path.display()
            ),
            Some(_) => {}
            None => {
                paths.insert(
                    key,
                    ZipPathNode {
                        path: ancestor.clone(),
                        kind: ZipEntryKind::Directory,
                        explicit: false,
                    },
                );
            }
        }
    }

    let key = zip_path_key(path);
    if let Some(existing) = paths.get_mut(&key) {
        match (existing.kind, kind, existing.explicit) {
            (ZipEntryKind::Directory, ZipEntryKind::Directory, false) => {
                existing.explicit = true;
                return Ok(());
            }
            (ZipEntryKind::Directory, ZipEntryKind::Directory, true) => {
                bail!(
                    "{context} duplicates directory '{}'",
                    existing.path.display()
                )
            }
            _ => bail!(
                "{context} collides with archive path '{}'",
                existing.path.display()
            ),
        }
    }

    if kind == ZipEntryKind::File {
        let prefix = format!("{key}/");
        if let Some(existing) = paths
            .iter()
            .find_map(|(candidate, node)| candidate.starts_with(&prefix).then_some(node))
        {
            bail!(
                "{context} collides with child path '{}'",
                existing.path.display()
            );
        }
    }

    paths.insert(
        key,
        ZipPathNode {
            path: path.to_path_buf(),
            kind,
            explicit: true,
        },
    );
    Ok(())
}

fn extract_zip_archive(
    archive_path: &Path,
    source_root: &Path,
    entries: &[ValidatedZipEntry],
    archive: &mut zip::ZipArchive<fs::File>,
    limits: ZipExtractionLimits,
) -> Result<()> {
    let canonical_root = canonical_path(source_root)?;
    let mut total_copied = 0_u64;

    for planned in entries {
        let destination = source_root.join(&planned.path);
        match planned.kind {
            ZipEntryKind::Directory => {
                ensure_zip_directory(source_root, &planned.path)?;
            }
            ZipEntryKind::File => {
                let relative_parent = planned
                    .path
                    .parent()
                    .expect("validated ZIP path has a parent");
                ensure_zip_directory(source_root, relative_parent)?;
                let parent = destination.parent().expect("ZIP output has a parent");
                let canonical_parent = canonical_path(parent)?;
                if !canonical_parent.starts_with(&canonical_root) {
                    bail!(
                        "ZIP archive '{}' would extract '{}' outside '{}'",
                        archive_path.display(),
                        planned.path.display(),
                        source_root.display()
                    );
                }
                let mut entry = archive.by_index(planned.index).with_context(|| {
                    format!(
                        "failed to read ZIP archive '{}' entry {}",
                        archive_path.display(),
                        planned.index
                    )
                })?;
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .with_context(|| {
                        format!("failed to create ZIP output {}", destination.display())
                    })?;
                let maximum_copy = planned
                    .declared_size
                    .min(limits.member_bytes)
                    .saturating_add(1);
                let copied = std::io::copy(&mut entry.by_ref().take(maximum_copy), &mut output)
                    .with_context(|| {
                        format!("failed to extract ZIP output {}", destination.display())
                    })?;
                total_copied = total_copied.checked_add(copied).with_context(|| {
                    format!(
                        "ZIP archive '{}' overflows the extracted byte total",
                        archive_path.display()
                    )
                })?;
                if copied != planned.declared_size {
                    bail!(
                        "ZIP archive '{}' entry '{}' extracted {} bytes but declared {}",
                        archive_path.display(),
                        planned.path.display(),
                        copied,
                        planned.declared_size
                    );
                }
                if total_copied > limits.total_bytes {
                    bail!(
                        "ZIP archive '{}' extracted beyond the {} byte total limit",
                        archive_path.display(),
                        limits.total_bytes
                    );
                }
                #[cfg(unix)]
                if let Some(mode) = planned.unix_mode {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&destination, fs::Permissions::from_mode(mode & 0o777))
                        .with_context(|| {
                            format!(
                                "failed to set ZIP output permissions {}",
                                destination.display()
                            )
                        })?;
                }
                output.sync_all().with_context(|| {
                    format!("failed to sync ZIP output {}", destination.display())
                })?;
            }
        }
        let canonical_destination = canonical_path(&destination)?;
        if !canonical_destination.starts_with(&canonical_root) {
            bail!(
                "ZIP archive '{}' extracted '{}' outside '{}'",
                archive_path.display(),
                planned.path.display(),
                source_root.display()
            );
        }
    }
    sync_directory_tree(source_root)?;
    Ok(())
}

fn sync_directory_tree(root: &Path) -> Result<()> {
    for entry in read_dir_sorted(root)? {
        let file_type = entry.file_type().with_context(|| {
            format!(
                "failed to inspect ZIP staging entry {}",
                entry.path().display()
            )
        })?;
        if file_type.is_symlink() {
            bail!(
                "ZIP staging tree unexpectedly contains symlink '{}'",
                entry.path().display()
            );
        }
        if file_type.is_dir() {
            sync_directory_tree(&entry.path())?;
        }
    }
    sync_directory(root)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("failed to open directory {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn ensure_zip_directory(source_root: &Path, relative: &Path) -> Result<()> {
    let mut directory = source_root.to_path_buf();
    for component in relative.components() {
        directory.push(component.as_os_str());
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => bail!(
                "ZIP output parent '{}' is not a real directory",
                directory.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&directory).with_context(|| {
                    format!("failed to create ZIP directory {}", directory.display())
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect ZIP directory {}", directory.display())
                });
            }
        }
    }
    Ok(())
}

fn create_zip_staging_root(source_root: &Path) -> Result<PathBuf> {
    let parent = source_root
        .parent()
        .with_context(|| format!("ZIP source root '{}' has no parent", source_root.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create ZIP source parent {}", parent.display()))?;
    let name = source_root
        .file_name()
        .unwrap_or_else(|| OsStr::new("source"))
        .to_string_lossy();
    for attempt in 0..1_024_u32 {
        let candidate = parent.join(format!(
            ".{name}.depos-staging-{}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(error) =
                        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700))
                    {
                        let _ = fs::remove_dir(&candidate);
                        return Err(error).with_context(|| {
                            format!(
                                "failed to secure ZIP staging directory {}",
                                candidate.display()
                            )
                        });
                    }
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create ZIP staging directory {}",
                        candidate.display()
                    )
                });
            }
        }
    }
    bail!(
        "unable to allocate a private ZIP staging directory beside '{}'",
        source_root.display()
    )
}

fn unused_zip_backup_path(source_root: &Path) -> Result<PathBuf> {
    let parent = source_root
        .parent()
        .with_context(|| format!("ZIP source root '{}' has no parent", source_root.display()))?;
    let name = source_root
        .file_name()
        .unwrap_or_else(|| OsStr::new("source"))
        .to_string_lossy();
    for attempt in 0..1_024_u32 {
        let candidate = parent.join(format!(
            ".{name}.depos-previous-{}-{attempt}",
            std::process::id()
        ));
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect ZIP backup path {}", candidate.display())
                });
            }
        }
    }
    bail!(
        "unable to allocate a ZIP backup path beside '{}'",
        source_root.display()
    )
}

fn publish_zip_source_tree(staging_root: &Path, source_root: &Path) -> Result<()> {
    let parent = source_root
        .parent()
        .with_context(|| format!("ZIP source root '{}' has no parent", source_root.display()))?;
    let previous_metadata = match fs::symlink_metadata(source_root) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect ZIP source root {}",
                    source_root.display()
                )
            });
        }
    };
    if previous_metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.file_type().is_dir() || metadata.file_type().is_symlink())
    {
        bail!(
            "ZIP source root '{}' must be a real directory",
            source_root.display()
        );
    }

    let backup = previous_metadata
        .is_some()
        .then(|| unused_zip_backup_path(source_root))
        .transpose()?;
    if let Some(backup) = &backup {
        fs::rename(source_root, backup).with_context(|| {
            format!(
                "failed to preserve prior ZIP source tree '{}' as '{}'",
                source_root.display(),
                backup.display()
            )
        })?;
        sync_directory(parent)?;
    }

    if let Err(install_error) = fs::rename(staging_root, source_root) {
        if let Some(backup) = &backup {
            if let Err(rollback_error) = fs::rename(backup, source_root) {
                bail!(
                    "failed to publish ZIP source tree '{}' to '{}': {}; rollback from '{}' also failed: {}",
                    staging_root.display(),
                    source_root.display(),
                    install_error,
                    backup.display(),
                    rollback_error
                );
            }
            sync_directory(parent)?;
        }
        return Err(install_error).with_context(|| {
            format!(
                "failed to publish ZIP source tree '{}' to '{}'",
                staging_root.display(),
                source_root.display()
            )
        });
    }
    sync_directory(parent)?;

    if let Some(backup) = backup {
        remove_existing_path(&backup).with_context(|| {
            format!(
                "failed to remove prior ZIP source tree {}",
                backup.display()
            )
        })?;
        sync_directory(parent)?;
    }
    Ok(())
}

fn copy_declared_exports(
    depos_root: &Path,
    source_root: &Path,
    store_root: &Path,
    spec: &PackageSpec,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    copy_declared_exports_from_candidates(
        depos_root,
        &[ExportCandidateRoot {
            label: "source",
            root: source_root,
        }],
        store_root,
        spec,
        log,
    )
}

fn apply_stage_entries(
    source_root: &Path,
    build_root: &Path,
    prefix_root: &Path,
    entries: &[StageEntry],
    log: &mut String,
) -> Result<()> {
    for entry in entries {
        let stage_root = match entry.source_root {
            StageSourceRoot::Source => source_root,
            StageSourceRoot::Build => build_root,
        };
        match entry.kind {
            StageKind::File => copy_install_path(
                stage_root,
                prefix_root,
                &entry.source,
                &entry.destination,
                false,
                log,
            )?,
            StageKind::Tree => copy_install_path(
                stage_root,
                prefix_root,
                &entry.source,
                &entry.destination,
                true,
                log,
            )?,
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ExportCandidateRoot<'a> {
    label: &'static str,
    root: &'a Path,
}

#[derive(Clone)]
struct ResolvedExportSource {
    root: PathBuf,
    relative_path: PathBuf,
    allow_directory: bool,
}

fn copy_declared_exports_from_candidates(
    depos_root: &Path,
    candidates: &[ExportCandidateRoot<'_>],
    store_root: &Path,
    spec: &PackageSpec,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    let resolved = resolve_declared_export_sources(candidates, spec)?;
    let mut copied_paths = Vec::new();
    for export in &resolved {
        copied_paths.extend(collect_export_paths(
            &export.root,
            &export.relative_path,
            export.allow_directory,
        )?);
    }
    let copied_paths = dedup_paths(copied_paths);
    assert_no_export_conflicts(depos_root, spec, store_root, &copied_paths)?;
    for export in resolved {
        copy_export_path(
            &export.root,
            store_root,
            &export.relative_path,
            export.allow_directory,
            log,
        )?;
    }
    Ok(copied_paths)
}

fn resolve_declared_export_sources(
    candidates: &[ExportCandidateRoot<'_>],
    spec: &PackageSpec,
) -> Result<Vec<ResolvedExportSource>> {
    let mut resolved = Vec::new();
    for artifact in &spec.artifacts {
        resolved.push(resolve_declared_export_source(candidates, artifact, false)?);
    }
    for target in &spec.targets {
        for include_dir in &target.include_dirs {
            resolved.push(resolve_declared_export_source(
                candidates,
                include_dir,
                true,
            )?);
        }
        for (_, path) in target.artifact_paths() {
            resolved.push(resolve_declared_export_source(candidates, path, false)?);
        }
    }
    Ok(dedup_resolved_export_sources(resolved))
}

fn resolve_declared_export_source(
    candidates: &[ExportCandidateRoot<'_>],
    relative_path: &Path,
    allow_directory: bool,
) -> Result<ResolvedExportSource> {
    let mut matches = Vec::new();
    for candidate in candidates {
        let candidate_path = candidate.root.join(relative_path);
        let metadata = match fs::symlink_metadata(&candidate_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", candidate_path.display()));
            }
        };
        ensure_resolved_path_within_root(
            &candidate_path,
            candidate.root,
            &format!(
                "declared export '{}' in {} output",
                relative_path.display(),
                candidate.label
            ),
        )?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() || file_type.is_file() {
            matches.push((candidate.label, candidate.root.to_path_buf()));
            continue;
        }
        if file_type.is_dir() {
            if !allow_directory {
                bail!(
                    "declared file export '{}' resolved to directory '{}' in {} output",
                    relative_path.display(),
                    candidate_path.display(),
                    candidate.label
                );
            }
            matches.push((candidate.label, candidate.root.to_path_buf()));
            continue;
        }
        bail!(
            "declared export '{}' resolved to unsupported file type '{}' in {} output",
            relative_path.display(),
            candidate_path.display(),
            candidate.label
        );
    }

    match matches.len() {
        0 => bail!(
            "declared export '{}' is missing from all candidate outputs ({})",
            relative_path.display(),
            candidates
                .iter()
                .map(|candidate| format!("{}={}", candidate.label, candidate.root.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        1 => {
            let (_, root) = matches.pop().unwrap();
            Ok(ResolvedExportSource {
                root,
                relative_path: relative_path.to_path_buf(),
                allow_directory,
            })
        }
        _ => bail!(
            "declared export '{}' exists in multiple candidate outputs ({}); add STAGE_FILE, STAGE_TREE, or an install phase to disambiguate",
            relative_path.display(),
            matches
                .iter()
                .map(|(label, root)| format!("{}={}", label, root.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn dedup_resolved_export_sources(entries: Vec<ResolvedExportSource>) -> Vec<ResolvedExportSource> {
    let mut deduped = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let key = format!(
            "{}|{}|{}",
            entry.root.display(),
            entry.relative_path.display(),
            entry.allow_directory
        );
        if seen.insert(key) {
            deduped.push(entry);
        }
    }
    deduped
}

fn collect_export_paths(
    source_root: &Path,
    relative_path: &Path,
    allow_directory: bool,
) -> Result<Vec<PathBuf>> {
    let source_path = source_root.join(relative_path);
    let metadata = fs::symlink_metadata(&source_path).with_context(|| {
        format!(
            "declared export '{}' is missing from fetched source '{}'",
            relative_path.display(),
            source_root.display()
        )
    })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() || file_type.is_file() {
        return Ok(vec![relative_path.to_path_buf()]);
    }
    if file_type.is_dir() {
        if !allow_directory {
            bail!(
                "declared file export '{}' resolved to a directory",
                source_path.display()
            );
        }
        return collect_directory_export_paths(&source_path, relative_path);
    }
    bail!(
        "declared export '{}' is not a regular file, directory, or symlink under '{}'",
        relative_path.display(),
        source_root.display()
    )
}

fn collect_directory_export_paths(source: &Path, relative_path: &Path) -> Result<Vec<PathBuf>> {
    let mut collected_paths = Vec::new();
    for entry in read_dir_sorted(source)? {
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let child_relative_path = relative_path.join(entry.file_name());
        if file_type.is_symlink() || file_type.is_file() {
            collected_paths.push(child_relative_path);
        } else if file_type.is_dir() {
            collected_paths.extend(collect_directory_export_paths(
                &source_path,
                &child_relative_path,
            )?);
        }
    }
    Ok(collected_paths)
}

fn assert_no_export_conflicts(
    depos_root: &Path,
    spec: &PackageSpec,
    store_root: &Path,
    planned_paths: &[PathBuf],
) -> Result<()> {
    let current_store_root = canonical_path(store_root)?;
    for (owner_name, owner_namespace, owner_version, manifest) in read_export_manifests(depos_root)?
    {
        if owner_name == spec.name
            && owner_namespace == spec.namespace
            && owner_version == spec.version
        {
            continue;
        }
        if manifest.store_root != current_store_root {
            continue;
        }
        if let Some((current_path, owned_path)) =
            find_export_path_conflict(planned_paths, &manifest.paths)
        {
            bail!(
                "package '{}' export '{}' conflicts with '{}[{}]@{}' owning '{}' under {}",
                spec.package_id(),
                current_path.display(),
                owner_name,
                owner_namespace,
                owner_version,
                owned_path.display(),
                manifest.store_root.display()
            );
        }
    }
    Ok(())
}

fn find_export_path_conflict<'a>(
    planned_paths: &'a [PathBuf],
    owned_paths: &'a [PathBuf],
) -> Option<(&'a Path, &'a Path)> {
    for planned_path in planned_paths {
        for owned_path in owned_paths {
            if export_paths_overlap(planned_path, owned_path) {
                return Some((planned_path.as_path(), owned_path.as_path()));
            }
        }
    }
    None
}

fn export_paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn copy_export_path(
    source_root: &Path,
    store_root: &Path,
    relative_path: &Path,
    allow_directory: bool,
    log: &mut String,
) -> Result<()> {
    let source_path = source_root.join(relative_path);
    let destination_path = store_root.join(relative_path);
    ensure_resolved_path_within_root(
        &source_path,
        source_root,
        &format!("declared export '{}'", relative_path.display()),
    )?;
    let metadata = fs::symlink_metadata(&source_path).with_context(|| {
        format!(
            "declared export '{}' is missing from fetched source '{}'",
            relative_path.display(),
            source_root.display()
        )
    })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        copy_symlink(&source_path, &destination_path, source_root, log)?;
        return Ok(());
    }
    if file_type.is_dir() {
        if !allow_directory {
            bail!(
                "declared file export '{}' resolved to a directory",
                source_path.display()
            );
        }
        log.push_str(&format!(
            "copy dir {} -> {}\n",
            source_path.display(),
            destination_path.display()
        ));
        copy_directory_recursive(
            &source_path,
            &destination_path,
            relative_path,
            source_root,
            log,
        )?;
        return Ok(());
    }
    if !file_type.is_file() {
        bail!(
            "declared export '{}' is not a regular file, directory, or symlink under '{}'",
            relative_path.display(),
            source_root.display()
        );
    }
    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    remove_existing_path(&destination_path)?;
    log.push_str(&format!(
        "copy file {} -> {}\n",
        source_path.display(),
        destination_path.display()
    ));
    fs::copy(&source_path, &destination_path).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source_path.display(),
            destination_path.display()
        )
    })?;
    Ok(())
}

fn copy_install_path(
    source_root: &Path,
    destination_root: &Path,
    source_relative_path: &Path,
    destination_relative_path: &Path,
    allow_directory: bool,
    log: &mut String,
) -> Result<()> {
    let source_path = source_root.join(source_relative_path);
    let destination_path = destination_root.join(destination_relative_path);
    ensure_resolved_path_within_root(
        &source_path,
        source_root,
        &format!("manual install source '{}'", source_relative_path.display()),
    )?;
    let metadata = fs::symlink_metadata(&source_path).with_context(|| {
        format!(
            "manual install source '{}' is missing from '{}'",
            source_relative_path.display(),
            source_root.display()
        )
    })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        copy_symlink(&source_path, &destination_path, source_root, log)?;
        return Ok(());
    }
    if file_type.is_dir() {
        if !allow_directory {
            bail!(
                "manual install file source '{}' resolved to a directory",
                source_path.display()
            );
        }
        log.push_str(&format!(
            "install dir {} -> {}\n",
            source_path.display(),
            destination_path.display()
        ));
        copy_directory_recursive_with_destination_root(
            &source_path,
            &destination_path,
            destination_relative_path,
            source_root,
            log,
        )?;
        return Ok(());
    }
    if !file_type.is_file() {
        bail!(
            "manual install source '{}' is not a regular file, directory, or symlink under '{}'",
            source_relative_path.display(),
            source_root.display()
        );
    }
    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    remove_existing_path(&destination_path)?;
    log.push_str(&format!(
        "install file {} -> {}\n",
        source_path.display(),
        destination_path.display()
    ));
    fs::copy(&source_path, &destination_path).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source_path.display(),
            destination_path.display()
        )
    })?;
    Ok(())
}

fn copy_directory_recursive(
    source: &Path,
    destination: &Path,
    relative_path: &Path,
    source_root: &Path,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    remove_existing_non_directory_path(destination)?;
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let mut copied_paths = Vec::new();
    for entry in read_dir_sorted(source)? {
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let file_name = entry.file_name();
        let destination_path = destination.join(&file_name);
        let child_relative_path = relative_path.join(&file_name);
        if file_type.is_symlink() {
            copy_symlink(&source_path, &destination_path, source_root, log)?;
            copied_paths.push(child_relative_path);
        } else if file_type.is_dir() {
            copied_paths.extend(copy_directory_recursive(
                &source_path,
                &destination_path,
                &child_relative_path,
                source_root,
                log,
            )?);
        } else if file_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            remove_existing_path(&destination_path)?;
            log.push_str(&format!(
                "copy file {} -> {}\n",
                source_path.display(),
                destination_path.display()
            ));
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
            copied_paths.push(child_relative_path);
        }
    }
    Ok(copied_paths)
}

fn copy_directory_recursive_with_destination_root(
    source: &Path,
    destination: &Path,
    destination_relative_path: &Path,
    source_root: &Path,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    remove_existing_non_directory_path(destination)?;
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let mut copied_paths = Vec::new();
    for entry in read_dir_sorted(source)? {
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let file_name = entry.file_name();
        let destination_path = destination.join(&file_name);
        let child_relative_path = destination_relative_path.join(&file_name);
        if file_type.is_symlink() {
            copy_symlink(&source_path, &destination_path, source_root, log)?;
            copied_paths.push(child_relative_path);
        } else if file_type.is_dir() {
            copied_paths.extend(copy_directory_recursive_with_destination_root(
                &source_path,
                &destination_path,
                &child_relative_path,
                source_root,
                log,
            )?);
        } else if file_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            remove_existing_path(&destination_path)?;
            log.push_str(&format!(
                "install file {} -> {}\n",
                source_path.display(),
                destination_path.display()
            ));
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
            copied_paths.push(child_relative_path);
        }
    }
    Ok(copied_paths)
}

fn copy_symlink(
    source: &Path,
    destination: &Path,
    source_root: &Path,
    log: &mut String,
) -> Result<()> {
    let target = validate_confined_symlink_target(source, source_root)?;
    let target_is_dir = fs::metadata(source)
        .with_context(|| format!("failed to inspect symlink target {}", source.display()))?
        .is_dir();
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    remove_existing_path(destination)?;
    log.push_str(&format!(
        "copy symlink {} -> {} ({})\n",
        source.display(),
        destination.display(),
        target.display()
    ));
    create_host_symlink(&target, destination, target_is_dir)?;
    Ok(())
}

fn create_host_symlink(target: &Path, destination: &Path, target_is_dir: bool) -> Result<()> {
    #[cfg(unix)]
    {
        let _ = target_is_dir;
        std::os::unix::fs::symlink(target, destination).with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                destination.display(),
                target.display()
            )
        })?;
    }
    #[cfg(windows)]
    {
        let result = if target_is_dir {
            std::os::windows::fs::symlink_dir(target, destination)
        } else {
            std::os::windows::fs::symlink_file(target, destination)
        };
        result.with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                destination.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

fn validate_confined_symlink_target(source: &Path, source_root: &Path) -> Result<PathBuf> {
    let target = fs::read_link(source)
        .with_context(|| format!("failed to read symlink {}", source.display()))?;
    let resolved_target = if target.is_absolute() {
        target.clone()
    } else {
        source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&target)
    };
    let canonical_target = canonical_path(&resolved_target).with_context(|| {
        format!(
            "failed to resolve symlink target '{}' from '{}'",
            target.display(),
            source.display()
        )
    })?;
    let canonical_root = canonical_path(source_root)?;
    if !canonical_target.starts_with(&canonical_root) {
        bail!(
            "symlink '{}' points outside its allowed root '{}': '{}'",
            source.display(),
            source_root.display(),
            target.display()
        );
    }
    Ok(target)
}

fn ensure_resolved_path_within_root(path: &Path, root: &Path, context: &str) -> Result<()> {
    let canonical_root = canonical_path(root)?;
    let canonical_resolved = canonical_path(path)
        .with_context(|| format!("failed to resolve '{}' at {}", context, path.display()))?;
    if !canonical_resolved.starts_with(&canonical_root) {
        bail!(
            "{} resolves outside its allowed root '{}': '{}'",
            context,
            root.display(),
            path.display()
        );
    }
    Ok(())
}

fn remove_existing_non_directory_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => remove_existing_path(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn remove_existing_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))
        }
        Ok(_) => {
            fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn hash_file_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to hash {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn run_command<I, S>(
    log: &mut String,
    current_dir: Option<&Path>,
    executable: &str,
    args: I,
    output_hint: Option<&Path>,
) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args_vec = args
        .into_iter()
        .map(|value| value.as_ref().to_string())
        .collect::<Vec<_>>();
    log.push_str(&format!("run {} {}\n", executable, args_vec.join(" ")));
    let executable_path = resolve_command_path(executable);

    let mut command = Command::new(&executable_path);
    command.args(&args_vec);
    if let Some(dir) = current_dir {
        command.current_dir(normalize_host_path(dir));
    }
    let output = command.output().with_context(|| {
        format!(
            "failed to spawn {}{}",
            executable_path.display(),
            output_hint
                .map(|path| format!(" for {}", path.display()))
                .unwrap_or_default()
        )
    })?;
    append_process_output(log, &output.stdout, &output.stderr);
    if !output.status.success() {
        let mut message = format!(
            "command '{}' failed with status {}",
            executable, output.status
        );
        append_process_failure_output(&mut message, "stdout", &output.stdout);
        append_process_failure_output(&mut message, "stderr", &output.stderr);
        bail!("{message}");
    }
    Ok(())
}

fn resolve_command_path(executable: &str) -> PathBuf {
    if executable.contains('/') || executable.contains('\\') {
        return PathBuf::from(executable);
    }
    if let Some(resolved) = find_executable_in_path(executable) {
        return resolved;
    }
    PathBuf::from(executable)
}

fn find_executable_in_path(executable: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let mut windows_extensions = Vec::new();
    let needs_windows_extension_search =
        cfg!(windows) && Path::new(executable).extension().is_none();
    if needs_windows_extension_search {
        windows_extensions = std::env::var_os("PATHEXT")
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
            });
    }
    for directory in std::env::split_paths(&path_var) {
        if needs_windows_extension_search {
            for extension in &windows_extensions {
                let candidate = directory.join(format!("{executable}{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        let direct = directory.join(executable);
        if direct.is_file() {
            return Some(direct);
        }
    }
    None
}

fn package_exports_present(spec: &PackageSpec, store_root: &Path) -> Result<bool> {
    for relative_path in spec.required_paths() {
        if !path_exists_or_symlink(&store_root.join(relative_path))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn registered_depofile_hash(depos_root: &Path, spec: &PackageSpec) -> Result<String> {
    let path =
        resolve_registered_depofile_path(depos_root, &spec.name, &spec.namespace, &spec.version)?;
    hash_file_sha256(&path)
}

fn dependency_materialization_keys(
    depos_root: &Path,
    spec: &PackageSpec,
    variant: &str,
) -> Result<Vec<(String, String)>> {
    let mut keys = resolve_dependency_specs(depos_root, spec)?
        .into_iter()
        .map(|dependency| {
            let package_id = dependency.package_id();
            let key = if dependency.origin == PackageOrigin::Local {
                read_materialization_state(
                    depos_root,
                    &dependency.name,
                    &dependency.namespace,
                    &dependency.version,
                    variant,
                )?
                .with_context(|| {
                    format!(
                        "local dependency '{}' is missing materialization state",
                        package_id
                    )
                })?
                .build_key
            } else {
                package_id.clone()
            };
            Ok((package_id, key))
        })
        .collect::<Result<Vec<_>>>()?;
    keys.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(keys)
}

fn materialization_build_key(
    depofile_hash: &str,
    provenance: &SourceProvenance,
    dependency_keys: &[(String, String)],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("depofile={depofile_hash}\n"));
    hasher.update(format!(
        "source_ref={}\n",
        provenance.source_ref.as_deref().unwrap_or("")
    ));
    hasher.update(format!(
        "source_commit={}\n",
        provenance.source_commit.as_deref().unwrap_or("")
    ));
    hasher.update(format!(
        "source_digest={}\n",
        provenance.source_digest.as_deref().unwrap_or("")
    ));
    for (package_id, key) in dependency_keys {
        hasher.update(format!("dependency={package_id}:{key}\n"));
    }
    format!("{:x}", hasher.finalize())
}

fn materialization_is_current(
    previous_state: Option<&MaterializationState>,
    previous_exports: Option<&ExportManifest>,
    store_root: &Path,
    depofile_hash: &str,
    build_key: &str,
    spec: &PackageSpec,
) -> Result<bool> {
    let (Some(previous_state), Some(previous_exports)) = (previous_state, previous_exports) else {
        return Ok(false);
    };
    if !store_root.exists() || !package_exports_present(spec, store_root)? {
        return Ok(false);
    }
    let canonical_store_root = canonical_path(store_root)?;
    Ok(previous_state.store_root == canonical_store_root
        && previous_exports.store_root == canonical_store_root
        && previous_state.depofile_hash == depofile_hash
        && previous_state.build_key == build_key)
}

fn write_materialization_status(
    depos_root: &Path,
    spec: &PackageSpec,
    state: PackageState,
    message: String,
) -> Result<()> {
    let provenance =
        read_source_provenance(depos_root, &spec.name, &spec.namespace, &spec.version)?;
    let status = PackageStatus {
        name: spec.name.clone(),
        namespace: spec.namespace.clone(),
        version: spec.version.clone(),
        lazy: spec.lazy,
        system_libs: spec.system_libs.clone(),
        state,
        depofile: resolve_registered_depofile_path(
            depos_root,
            &spec.name,
            &spec.namespace,
            &spec.version,
        )?,
        message,
        source_ref: provenance.source_ref,
        source_commit: provenance.source_commit,
    };
    write_status_file(
        depos_root,
        &status,
        &variant_for_target(&spec.target_arch, spec.target_platform)?,
    )
}

fn write_materialization_log(depos_root: &Path, spec: &PackageSpec, log: &str) -> Result<()> {
    let path = log_file_path(
        depos_root,
        &spec.name,
        &spec.namespace,
        &spec.version,
        &variant_for_target(&spec.target_arch, spec.target_platform)?,
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid log path {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(&path, log).with_context(|| format!("failed to write {}", path.display()))
}

fn reconcile_export_manifest(
    store_root: &Path,
    previous: Option<&ExportManifest>,
    current_paths: &[PathBuf],
    log: &mut String,
) -> Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let current_paths = current_paths.iter().cloned().collect::<BTreeSet<_>>();
    let current_store_root = canonical_path(store_root)?;
    let previous_store_root = if previous.store_root.exists() {
        canonical_path(&previous.store_root)?
    } else {
        previous.store_root.clone()
    };
    let stale_paths = if previous_store_root == current_store_root {
        previous
            .paths
            .iter()
            .filter(|path| !current_paths.contains(*path))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        previous.paths.clone()
    };
    remove_exported_paths(&previous_store_root, &stale_paths, log)
}

fn write_export_manifest(
    depos_root: &Path,
    spec: &PackageSpec,
    variant: &str,
    store_root: &Path,
    paths: &[PathBuf],
) -> Result<()> {
    let path = export_manifest_path(
        depos_root,
        &spec.name,
        &spec.namespace,
        &spec.version,
        variant,
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid export manifest path {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut contents = format!(
        "STORE_ROOT {}\n",
        display_path(&canonical_path(store_root)?)
    );
    for relative_path in dedup_paths(paths.to_vec()) {
        contents.push_str("PATH ");
        contents.push_str(&display_path(&relative_path));
        contents.push('\n');
    }
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn read_export_manifest(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
    variant: &str,
) -> Result<Option<ExportManifest>> {
    let path = export_manifest_path(depos_root, name, namespace, version, variant);
    if !path.exists() {
        return Ok(None);
    }
    let manifest = read_export_manifest_path(&path)?;
    if !store_roots_match(
        &manifest.store_root,
        &variant_package_store_root(depos_root, variant, name, namespace, version),
    )? {
        return Ok(None);
    }
    Ok(Some(manifest))
}

fn read_export_manifest_path(path: &Path) -> Result<ExportManifest> {
    let source =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut store_root = None;
    let mut paths = Vec::new();
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("STORE_ROOT ") {
            let value = PathBuf::from(value);
            if !value.is_absolute() {
                bail!(
                    "STORE_ROOT in {} must be absolute, got '{}'",
                    path.display(),
                    value.display()
                );
            }
            store_root = Some(value);
            continue;
        }
        if let Some(value) = line.strip_prefix("PATH ") {
            let value = PathBuf::from(value);
            ensure_export_manifest_path(&value, path)?;
            paths.push(value);
            continue;
        }
        bail!(
            "unsupported export manifest syntax at {}: {}",
            path.display(),
            raw_line
        );
    }
    Ok(ExportManifest {
        store_root: store_root
            .with_context(|| format!("missing STORE_ROOT in {}", path.display()))?,
        paths: dedup_paths(paths),
    })
}

fn variant_metadata_paths_for_package(
    depos_root: &Path,
    category: &str,
    extension: &str,
    name: &str,
    namespace: &str,
    version: &str,
) -> Result<Vec<VariantMetadataPath>> {
    let root = depos_root
        .join(".run")
        .join(category)
        .join(name)
        .join(namespace);
    let mut paths = Vec::new();
    let legacy = root.join(format!("{version}.{extension}"));
    if legacy.is_file() {
        paths.push(VariantMetadataPath {
            path: legacy,
            variant: default_variant(),
        });
    }
    let variants = root.join(version);
    if variants.is_dir() {
        for entry in read_dir_sorted(&variants)? {
            let path = entry.path();
            if entry.file_type()?.is_file()
                && path.extension().and_then(OsStr::to_str) == Some(extension)
            {
                let variant = path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .with_context(|| format!("invalid variant metadata name {}", path.display()))?
                    .to_string();
                ensure_safe_path_component(&variant, "variant")?;
                paths.push(VariantMetadataPath { path, variant });
            }
        }
    }
    Ok(paths)
}

fn read_export_manifests(
    depos_root: &Path,
) -> Result<Vec<(String, String, String, ExportManifest)>> {
    let root = depos_root.join(".run").join("exports");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut manifests = Vec::new();
    for name_entry in read_dir_sorted(&root)? {
        if !name_entry.file_type()?.is_dir() {
            continue;
        }
        let name = name_entry.file_name().to_string_lossy().into_owned();
        for namespace_entry in read_dir_sorted(&name_entry.path())? {
            if !namespace_entry.file_type()?.is_dir() {
                continue;
            }
            let namespace = namespace_entry.file_name().to_string_lossy().into_owned();
            for version_entry in read_dir_sorted(&namespace_entry.path())? {
                let version_path = version_entry.path();
                if version_entry.file_type()?.is_file() {
                    if version_path.extension().and_then(OsStr::to_str) != Some("exports") {
                        continue;
                    }
                    let Some(version) = version_path
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    manifests.push((
                        name.clone(),
                        namespace.clone(),
                        version,
                        read_export_manifest_path(&version_path)?,
                    ));
                    continue;
                }
                if !version_entry.file_type()?.is_dir() {
                    continue;
                }
                let version = version_entry.file_name().to_string_lossy().into_owned();
                for variant_entry in read_dir_sorted(&version_path)? {
                    let variant_path = variant_entry.path();
                    if !variant_entry.file_type()?.is_file()
                        || variant_path.extension().and_then(OsStr::to_str) != Some("exports")
                    {
                        continue;
                    }
                    manifests.push((
                        name.clone(),
                        namespace.clone(),
                        version.clone(),
                        read_export_manifest_path(&variant_path)?,
                    ));
                }
            }
        }
    }
    Ok(manifests)
}

fn write_source_provenance(
    depos_root: &Path,
    spec: &PackageSpec,
    provenance: &SourceProvenance,
) -> Result<()> {
    let path = provenance_file_path(depos_root, &spec.name, &spec.namespace, &spec.version);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid provenance path {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let mut contents = String::new();
    if let Some(source_ref) = &provenance.source_ref {
        contents.push_str("source_ref=");
        contents.push_str(source_ref);
        contents.push('\n');
    }
    if let Some(source_commit) = &provenance.source_commit {
        contents.push_str("source_commit=");
        contents.push_str(source_commit);
        contents.push('\n');
    }
    if let Some(source_digest) = &provenance.source_digest {
        contents.push_str("source_digest=");
        contents.push_str(source_digest);
        contents.push('\n');
    }
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn read_source_provenance(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
) -> Result<SourceProvenance> {
    let path = provenance_file_path(depos_root, name, namespace, version);
    if !path.exists() {
        return Ok(SourceProvenance::default());
    }
    let source =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut provenance = SourceProvenance::default();
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("invalid provenance syntax at {}", path.display()))?;
        match key {
            "source_ref" => provenance.source_ref = Some(value.to_string()),
            "source_commit" => provenance.source_commit = Some(value.to_string()),
            "source_digest" => provenance.source_digest = Some(value.to_string()),
            _ => bail!("unsupported provenance key '{}' in {}", key, path.display()),
        }
    }
    Ok(provenance)
}

fn write_materialization_state(
    depos_root: &Path,
    spec: &PackageSpec,
    variant: &str,
    store_root: &Path,
    depofile_hash: &str,
    build_key: &str,
) -> Result<()> {
    let path = materialization_state_path(
        depos_root,
        &spec.name,
        &spec.namespace,
        &spec.version,
        variant,
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid materialization state path {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let contents = format!(
        "store_root={}\ndepofile_hash={}\nbuild_key={}\n",
        display_path(&canonical_path(store_root)?),
        depofile_hash,
        build_key
    );
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn read_materialization_state(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
    variant: &str,
) -> Result<Option<MaterializationState>> {
    let path = materialization_state_path(depos_root, name, namespace, version, variant);
    if !path.exists() {
        return Ok(None);
    }
    let source =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut store_root = None;
    let mut depofile_hash = None;
    let mut build_key = None;
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').with_context(|| {
            format!("invalid materialization state syntax at {}", path.display())
        })?;
        match key {
            "store_root" => store_root = Some(PathBuf::from(value)),
            "depofile_hash" => depofile_hash = Some(value.to_string()),
            "build_key" => build_key = Some(value.to_string()),
            _ => bail!(
                "unsupported materialization state key '{}' in {}",
                key,
                path.display()
            ),
        }
    }
    let state = MaterializationState {
        store_root: store_root
            .with_context(|| format!("missing store_root in {}", path.display()))?,
        depofile_hash: depofile_hash
            .with_context(|| format!("missing depofile_hash in {}", path.display()))?,
        build_key: build_key.with_context(|| format!("missing build_key in {}", path.display()))?,
    };
    if !store_roots_match(
        &state.store_root,
        &variant_package_store_root(depos_root, variant, name, namespace, version),
    )? {
        return Ok(None);
    }
    Ok(Some(state))
}

fn remove_exported_paths(
    store_root: &Path,
    relative_paths: &[PathBuf],
    log: &mut String,
) -> Result<()> {
    if relative_paths.is_empty() {
        return Ok(());
    }
    let store_root = canonical_path(store_root)?;
    for relative_path in relative_paths {
        ensure_export_manifest_path(relative_path, Path::new("<loaded export manifest>"))?;
        let absolute_path = store_root.join(relative_path);
        if !path_exists_or_symlink(&absolute_path)? {
            continue;
        }
        let parent = canonical_path(
            absolute_path
                .parent()
                .ok_or_else(|| anyhow!("invalid export path {}", absolute_path.display()))?,
        )?;
        if !parent.starts_with(&store_root) {
            bail!(
                "refusing to remove export '{}' through parent outside store root '{}'",
                absolute_path.display(),
                store_root.display()
            );
        }
        log.push_str(&format!(
            "remove stale export {}\n",
            absolute_path.display()
        ));
        remove_existing_path(&absolute_path)?;
        prune_empty_ancestors(&parent, &store_root)?;
    }
    Ok(())
}

fn confined_existing_parent(
    depos_root: &Path,
    owner_root: &Path,
    path: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let depos_root = canonical_path(depos_root)?;
    if fs::symlink_metadata(owner_root)
        .with_context(|| format!("failed to inspect {}", owner_root.display()))?
        .file_type()
        .is_symlink()
    {
        bail!(
            "refusing to traverse symlinked metadata root '{}'",
            owner_root.display()
        );
    }
    let owner_root = canonical_path(owner_root)?;
    if owner_root != depos_root && !owner_root.starts_with(&depos_root) {
        bail!(
            "refusing to access metadata root '{}' outside Depos root '{}'",
            owner_root.display(),
            depos_root.display()
        );
    }
    let parent = canonical_path(
        path.parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?,
    )?;
    if parent != owner_root && !parent.starts_with(&owner_root) {
        bail!(
            "refusing to access '{}' through parent outside metadata root '{}'",
            path.display(),
            owner_root.display()
        );
    }
    Ok((parent, owner_root))
}

fn remove_confined_path(depos_root: &Path, owner_root: &Path, path: &Path) -> Result<()> {
    if !path_exists_or_symlink(path)? {
        return Ok(());
    }
    let (parent, owner_root) = confined_existing_parent(depos_root, owner_root, path)?;
    remove_existing_path(path)?;
    prune_empty_ancestors(&parent, &owner_root)
}

fn destructive_store_root(manifest: &Path, expected: &Path) -> Result<PathBuf> {
    let manifest = canonical_path(manifest).with_context(|| {
        format!(
            "failed to verify manifest store root {}",
            manifest.display()
        )
    })?;
    let expected = canonical_path(expected).with_context(|| {
        format!(
            "failed to verify expected store root {}",
            expected.display()
        )
    })?;
    if manifest != expected {
        bail!(
            "export manifest store root '{}' does not match expected root '{}'",
            manifest.display(),
            expected.display()
        );
    }
    Ok(expected)
}

pub fn register_depofile(options: &RegisterOptions) -> Result<PackageStatus> {
    let depos_root = resolve_depos_root(&options.depos_root)?;
    let source_file = canonical_path(&options.file)?;
    ensure_namespace_name(&options.namespace)?;
    let spec = parse_depofile(&source_file)?;

    let registered_path =
        registered_depofile_path(&depos_root, &spec.name, &options.namespace, &spec.version);
    let registered_dir = registered_path
        .parent()
        .ok_or_else(|| anyhow!("invalid registered DepoFile path"))?;
    fs::create_dir_all(registered_dir)
        .with_context(|| format!("failed to create {}", registered_dir.display()))?;
    fs::copy(&source_file, &registered_path).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source_file.display(),
            registered_path.display()
        )
    })?;

    refresh_status(&depos_root, &spec.name, &options.namespace, &spec.version)
}

pub fn unregister_depofile(options: &UnregisterOptions) -> Result<()> {
    let depos_root = resolve_depos_root(&options.depos_root)?;
    ensure_package_name(&options.name)?;
    ensure_namespace_name(&options.namespace)?;
    ensure_version_name(&options.version)?;
    let export_manifests = variant_metadata_paths_for_package(
        &depos_root,
        "exports",
        "exports",
        &options.name,
        &options.namespace,
        &options.version,
    )?;
    let export_root = depos_root.join(".run").join("exports");
    for export_manifest in &export_manifests {
        confined_existing_parent(&depos_root, &export_root, &export_manifest.path)?;
        let manifest = read_export_manifest_path(&export_manifest.path)?;
        let expected_store_root = variant_package_store_root(
            &depos_root,
            &export_manifest.variant,
            &options.name,
            &options.namespace,
            &options.version,
        );
        let store_root = destructive_store_root(&manifest.store_root, &expected_store_root)?;
        let mut cleanup_log = String::new();
        remove_exported_paths(&store_root, &manifest.paths, &mut cleanup_log)?;
    }
    for export_manifest in export_manifests {
        remove_confined_path(&depos_root, &export_root, &export_manifest.path)?;
    }

    let depofile_root = depos_root.join("depofiles").join("local");
    let depofile_dir = depos_root
        .join("depofiles")
        .join("local")
        .join(&options.name)
        .join(&options.namespace)
        .join(&options.version);
    remove_confined_path(&depos_root, &depofile_root, &depofile_dir)?;

    let status_root = depos_root.join(".run").join("status");
    let status_paths = variant_metadata_paths_for_package(
        &depos_root,
        "status",
        "status",
        &options.name,
        &options.namespace,
        &options.version,
    )?;
    for status in status_paths {
        remove_confined_path(&depos_root, &status_root, &status.path)?;
    }

    let log_root = depos_root.join(".run").join("logs");
    let log_paths = variant_metadata_paths_for_package(
        &depos_root,
        "logs",
        "log",
        &options.name,
        &options.namespace,
        &options.version,
    )?;
    for log in log_paths {
        remove_confined_path(&depos_root, &log_root, &log.path)?;
    }

    let provenance_root = depos_root.join(".run").join("provenance");
    let provenance_path = provenance_file_path(
        &depos_root,
        &options.name,
        &options.namespace,
        &options.version,
    );
    remove_confined_path(&depos_root, &provenance_root, &provenance_path)?;

    let materialization_root = depos_root.join(".run").join("materialization");
    let materialization_state_paths = variant_metadata_paths_for_package(
        &depos_root,
        "materialization",
        "state",
        &options.name,
        &options.namespace,
        &options.version,
    )?;
    for materialization_state in materialization_state_paths {
        remove_confined_path(
            &depos_root,
            &materialization_root,
            &materialization_state.path,
        )?;
    }

    Ok(())
}

pub fn collect_statuses(options: &StatusOptions) -> Result<Vec<PackageStatus>> {
    collect_statuses_for_target_platform(options, TargetPlatform::Host)
}

pub fn collect_statuses_for_target_platform(
    options: &StatusOptions,
    target_platform: TargetPlatform,
) -> Result<Vec<PackageStatus>> {
    target_platform.validate_host()?;
    let depos_root = resolve_depos_root(&options.depos_root)?;

    match (&options.name, &options.namespace, &options.version) {
        (Some(name), namespace, Some(version)) => {
            let namespace = namespace.clone().unwrap_or_else(default_namespace);
            ensure_package_name(name)?;
            ensure_namespace_name(&namespace)?;
            ensure_version_name(version)?;
            let status = if options.refresh {
                refresh_status_for_target_platform(
                    &depos_root,
                    name,
                    &namespace,
                    version,
                    target_platform,
                )?
            } else {
                read_or_refresh_status_for_target_platform(
                    &depos_root,
                    name,
                    &namespace,
                    version,
                    target_platform,
                )?
            };
            Ok(vec![status])
        }
        (None, None, None) => {
            let mut statuses = Vec::new();
            for (name, namespace, version) in registered_packages(&depos_root)? {
                let status = if options.refresh {
                    refresh_status_for_target_platform(
                        &depos_root,
                        &name,
                        &namespace,
                        &version,
                        target_platform,
                    )?
                } else {
                    read_or_refresh_status_for_target_platform(
                        &depos_root,
                        &name,
                        &namespace,
                        &version,
                        target_platform,
                    )?
                };
                statuses.push(status);
            }
            statuses.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.namespace.cmp(&right.namespace))
                    .then_with(|| left.version.cmp(&right.version))
            });
            Ok(statuses)
        }
        _ => {
            bail!("status requires --name with --version and optional --namespace, or none of them")
        }
    }
}

pub fn parse_manifest(path: &Path) -> Result<Vec<PackageRequest>> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut requests = Vec::new();

    for line in significant_lines(&source) {
        if !line.text.starts_with("depos_require(") || !line.text.ends_with(')') {
            bail!(
                "unsupported manifest syntax at {}:{}: {}",
                path.display(),
                line.number,
                line.text
            );
        }

        let inner = &line.text["depos_require(".len()..line.text.len() - 1];
        let tokens = tokenize_arguments(inner).with_context(|| {
            format!(
                "invalid manifest syntax at {}:{}",
                path.display(),
                line.number
            )
        })?;
        if tokens.is_empty() {
            bail!(
                "manifest request at {}:{} is empty",
                path.display(),
                line.number
            );
        }
        let name = tokens[0].clone();
        ensure_package_name(&name)?;

        let mut index = 1usize;
        let mut namespace = default_namespace();
        let mut mode = RequestMode::Latest;
        let mut source = RequestSource::Auto;
        let mut alias = None;
        while index < tokens.len() {
            match tokens[index].as_str() {
                "NAMESPACE" => {
                    index += 1;
                    let value = tokens.get(index).context("NAMESPACE requires a value")?;
                    ensure_namespace_name(value)?;
                    namespace = value.clone();
                }
                "VERSION" => {
                    index += 1;
                    let value = tokens.get(index).context("VERSION requires a value")?;
                    ensure_version_name(value)?;
                    mode = RequestMode::Exact(value.clone());
                }
                "MIN_VERSION" => {
                    index += 1;
                    let value = tokens.get(index).context("MIN_VERSION requires a value")?;
                    ensure_version_name(value)?;
                    mode = RequestMode::Minimum(value.clone());
                }
                "SOURCE" => {
                    index += 1;
                    let value = tokens.get(index).context("SOURCE requires a value")?;
                    source = RequestSource::parse(value)?;
                }
                "AS" => {
                    index += 1;
                    let value = tokens.get(index).context("AS requires a value")?;
                    ensure_alias_name(value)?;
                    alias = Some(value.clone());
                }
                other => bail!(
                    "unsupported manifest token '{}' at {}:{}",
                    other,
                    path.display(),
                    line.number
                ),
            }
            index += 1;
        }

        requests.push(PackageRequest {
            name,
            namespace,
            inherit_namespace: false,
            mode,
            source,
            alias,
        });
    }

    Ok(requests)
}

pub fn parse_depofile(path: &Path) -> Result<PackageSpec> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;

    let mut name: Option<String> = None;
    let namespace = default_namespace();
    let mut version: Option<String> = None;
    let mut primary_target_name: Option<String> = None;
    let mut source_subdir: Option<PathBuf> = None;
    let mut lazy = false;
    let mut system_libs = PackageSystemLibs::Inherit;
    let mut artifacts = Vec::new();
    let mut targets = Vec::new();
    let mut pending_target_links = BTreeMap::<String, Vec<String>>::new();
    let mut pending_target_definitions = BTreeMap::<String, Vec<String>>::new();
    let mut pending_target_options = BTreeMap::<String, Vec<String>>::new();
    let mut pending_target_features = BTreeMap::<String, Vec<String>>::new();
    let mut depends = Vec::new();
    let mut fetch: Option<FetchSpec> = None;
    let mut configure = Vec::new();
    let mut build = Vec::new();
    let mut install = Vec::new();
    let mut stage_entries = Vec::new();
    let mut build_root = BuildRoot::System;
    let mut toolchain = ToolchainSource::System;
    let mut toolchain_inputs = Vec::new();
    let mut build_arch = host_arch();
    let mut target_arch = build_arch.clone();
    let mut supported_target_platforms = BTreeSet::from([TargetPlatform::Host]);
    let mut target_platforms_declared = false;
    let mut build_system: Option<BuildSystem> = None;
    let mut build_system_line = None;
    let mut git_submodules_recursive = false;
    let mut cmake_args = Vec::new();
    let mut cmake_defines = Vec::new();
    let mut meson_args = Vec::new();
    let mut meson_defines = Vec::new();
    let mut autoconf_args = Vec::new();
    let mut autoconf_skip_configure = false;
    let mut cargo_build_args = Vec::new();
    let mut cargo_install_args = Vec::new();
    let mut cmake_configure_sh = None;
    let mut cmake_build_sh = None;
    let mut cmake_install_sh = None;
    let mut cmake_configure = None;
    let mut cmake_build = None;
    let mut cmake_install = None;
    let mut meson_setup_sh = None;
    let mut meson_compile_sh = None;
    let mut meson_install_sh = None;
    let mut meson_setup = None;
    let mut meson_compile = None;
    let mut meson_install = None;
    let mut autoconf_configure_sh = None;
    let mut autoconf_build_sh = None;
    let mut autoconf_install_sh = None;
    let mut autoconf_configure = None;
    let mut autoconf_build = None;
    let mut autoconf_install = None;
    let mut cargo_build_sh = None;
    let mut cargo_install_sh = None;
    let mut cargo_build = None;
    let mut cargo_install = None;
    let mut manual_prepare_sh = None;
    let mut manual_build_sh = None;
    let mut manual_install_sh = None;
    let mut manual_prepare = None;
    let mut manual_build = None;
    let mut manual_install = None;

    for directive in depofile_directives(path, &source)? {
        let keyword = directive.keyword.as_str();
        let remainder = directive.remainder.as_str();
        let line_number = directive.line_number;
        match keyword {
            "NAME" => {
                let value = expect_single_token(path, line_number, keyword, remainder)?;
                ensure_package_name(&value)?;
                name = Some(value);
            }
            "NAMESPACE" => {
                bail!(
                    "{}:{}: NAMESPACE is assigned when the DepoFile is registered; use `depos register --namespace ...` instead",
                    path.display(),
                    line_number
                );
            }
            "VERSION" => {
                let value = expect_single_token(path, line_number, keyword, remainder)?;
                ensure_version_name(&value)?;
                version = Some(value);
            }
            "SOURCE_SUBDIR" => {
                source_subdir = Some(PathBuf::from(expect_single_token(
                    path,
                    line_number,
                    keyword,
                    remainder,
                )?));
            }
            "LAZY" => {
                ensure_empty(path, line_number, keyword, remainder)?;
                lazy = true;
            }
            "SYSTEM_LIBS" => {
                let value = expect_single_token(path, line_number, keyword, remainder)?;
                system_libs = PackageSystemLibs::parse(&value)?;
            }
            "SOURCE" => {
                let parts = tokenize_arguments(remainder)?;
                match parts.as_slice() {
                    [kind, url] if kind == "URL" => {
                        fetch = Some(FetchSpec::Url {
                            url: url.clone(),
                            sha256: None,
                        });
                    }
                    [kind, url] if kind == "GIT" => {
                        fetch = Some(FetchSpec::Git {
                            url: url.clone(),
                            reference: "HEAD".to_string(),
                        });
                    }
                    [kind, url, reference] if kind == "GIT" => {
                        fetch = Some(FetchSpec::Git {
                            url: url.clone(),
                            reference: reference.clone(),
                        });
                    }
                    _ => bail!(
                        "{}:{}: SOURCE requires 'URL <url>' or 'GIT <url> [branch|tag|commit]'",
                        path.display(),
                        line_number
                    ),
                }
            }
            "ARTIFACT" => {
                artifacts.push(PathBuf::from(expect_single_token(
                    path,
                    line_number,
                    keyword,
                    remainder,
                )?));
            }
            "TARGET" => {
                parse_target_line(path, line_number, remainder, &mut targets)?;
            }
            "PRIMARY_TARGET" => {
                let value = expect_single_token(path, line_number, keyword, remainder)?;
                if primary_target_name.is_some() {
                    bail!(
                        "{}:{}: PRIMARY_TARGET is already set",
                        path.display(),
                        line_number
                    );
                }
                primary_target_name = Some(value);
            }
            "LINK" => {
                let parts = tokenize_arguments(remainder)?;
                if parts.len() < 2 {
                    bail!(
                        "{}:{}: LINK requires '<target> <item>...'",
                        path.display(),
                        line_number
                    );
                }
                pending_target_links
                    .entry(parts[0].clone())
                    .or_default()
                    .extend(parts[1..].iter().cloned());
            }
            "DEFINES" => {
                parse_target_values_directive(
                    path,
                    line_number,
                    keyword,
                    remainder,
                    &mut pending_target_definitions,
                )?;
            }
            "OPTIONS" => {
                parse_target_values_directive(
                    path,
                    line_number,
                    keyword,
                    remainder,
                    &mut pending_target_options,
                )?;
            }
            "FEATURES" => {
                parse_target_values_directive(
                    path,
                    line_number,
                    keyword,
                    remainder,
                    &mut pending_target_features,
                )?;
            }
            "SHA256" => {
                let value = expect_single_token(path, line_number, keyword, remainder)?;
                if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    bail!(
                        "{}:{}: SHA256 must be a 64-character hex digest",
                        path.display(),
                        line_number
                    );
                }
                match fetch.as_mut() {
                    Some(FetchSpec::Url { sha256, .. }) => *sha256 = Some(value),
                    Some(FetchSpec::Git { .. }) => {
                        bail!(
                            "{}:{}: SHA256 is only valid for SOURCE URL",
                            path.display(),
                            line_number
                        )
                    }
                    None => bail!(
                        "{}:{}: SHA256 requires a preceding SOURCE URL",
                        path.display(),
                        line_number
                    ),
                }
            }
            "GIT_SUBMODULES" => {
                let value = expect_single_token(path, line_number, keyword, remainder)?;
                match value.as_str() {
                    "RECURSIVE" => match fetch.as_ref() {
                        Some(FetchSpec::Git { .. }) => git_submodules_recursive = true,
                        Some(FetchSpec::Url { .. }) => bail!(
                            "{}:{}: GIT_SUBMODULES requires a preceding SOURCE GIT, not SOURCE URL",
                            path.display(),
                            line_number
                        ),
                        None => bail!(
                            "{}:{}: GIT_SUBMODULES requires a preceding SOURCE GIT",
                            path.display(),
                            line_number
                        ),
                    },
                    _ => bail!(
                        "{}:{}: GIT_SUBMODULES requires RECURSIVE",
                        path.display(),
                        line_number
                    ),
                }
            }
            "BUILD_SYSTEM" => {
                let selected = parse_build_system(path, line_number, remainder)?;
                if let Some(existing) = build_system {
                    bail!(
                        "{}:{}: BUILD_SYSTEM is already set to {} at line {}",
                        path.display(),
                        line_number,
                        existing.directive_name(),
                        build_system_line.unwrap_or(line_number)
                    );
                }
                build_system = Some(selected);
                build_system_line = Some(line_number);
            }
            "CMAKE_ARG" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cmake)?;
                cmake_args.push(expect_single_token(path, line_number, keyword, remainder)?);
            }
            "CMAKE_DEFINE" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cmake)?;
                cmake_defines.push(expect_single_token(path, line_number, keyword, remainder)?);
            }
            "MESON_ARG" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Meson)?;
                meson_args.push(expect_single_token(path, line_number, keyword, remainder)?);
            }
            "MESON_DEFINE" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Meson)?;
                meson_defines.push(expect_single_token(path, line_number, keyword, remainder)?);
            }
            "AUTOCONF_ARG" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Autoconf,
                )?;
                autoconf_args.push(expect_single_token(path, line_number, keyword, remainder)?);
            }
            "AUTOCONF_SKIP_CONFIGURE" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Autoconf,
                )?;
                ensure_empty(path, line_number, keyword, remainder)?;
                autoconf_skip_configure = true;
            }
            "CARGO_BUILD_ARG" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cargo)?;
                cargo_build_args.push(expect_single_token(path, line_number, keyword, remainder)?);
            }
            "CARGO_INSTALL_ARG" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cargo)?;
                cargo_install_args.push(expect_single_token(
                    path,
                    line_number,
                    keyword,
                    remainder,
                )?);
            }
            "CMAKE_CONFIGURE_SH" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cmake)?;
                cmake_configure_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "CMAKE_CONFIGURE" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cmake)?;
                cmake_configure = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "CMAKE_BUILD_SH" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cmake)?;
                cmake_build_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "CMAKE_BUILD" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cmake)?;
                cmake_build = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "CMAKE_INSTALL_SH" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cmake)?;
                cmake_install_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "CMAKE_INSTALL" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cmake)?;
                cmake_install = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "MESON_SETUP_SH" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Meson)?;
                meson_setup_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "MESON_SETUP" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Meson)?;
                meson_setup = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "MESON_COMPILE_SH" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Meson)?;
                meson_compile_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "MESON_COMPILE" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Meson)?;
                meson_compile = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "MESON_INSTALL_SH" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Meson)?;
                meson_install_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "MESON_INSTALL" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Meson)?;
                meson_install = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "AUTOCONF_CONFIGURE_SH" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Autoconf,
                )?;
                autoconf_configure_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "AUTOCONF_CONFIGURE" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Autoconf,
                )?;
                autoconf_configure =
                    Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "AUTOCONF_BUILD_SH" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Autoconf,
                )?;
                autoconf_build_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "AUTOCONF_BUILD" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Autoconf,
                )?;
                autoconf_build = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "AUTOCONF_INSTALL_SH" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Autoconf,
                )?;
                autoconf_install_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "AUTOCONF_INSTALL" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Autoconf,
                )?;
                autoconf_install =
                    Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "CARGO_BUILD_SH" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cargo)?;
                cargo_build_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "CARGO_BUILD" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cargo)?;
                cargo_build = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "CARGO_INSTALL_SH" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cargo)?;
                cargo_install_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "CARGO_INSTALL" => {
                require_build_system(path, line_number, keyword, build_system, BuildSystem::Cargo)?;
                cargo_install = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "MANUAL_PREPARE_SH" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Manual,
                )?;
                manual_prepare_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "MANUAL_PREPARE" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Manual,
                )?;
                manual_prepare = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "MANUAL_BUILD_SH" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Manual,
                )?;
                manual_build_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "MANUAL_BUILD" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Manual,
                )?;
                manual_build = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "MANUAL_INSTALL_SH" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Manual,
                )?;
                manual_install_sh = Some(block_body(
                    path,
                    line_number,
                    keyword,
                    directive.block.as_ref(),
                )?);
            }
            "MANUAL_INSTALL" => {
                require_build_system(
                    path,
                    line_number,
                    keyword,
                    build_system,
                    BuildSystem::Manual,
                )?;
                manual_install = Some(parse_phase_command(path, line_number, keyword, remainder)?);
            }
            "STAGE_FILE" => {
                stage_entries.push(parse_stage_entry(
                    path,
                    line_number,
                    remainder,
                    StageKind::File,
                )?);
            }
            "STAGE_TREE" => {
                stage_entries.push(parse_stage_entry(
                    path,
                    line_number,
                    remainder,
                    StageKind::Tree,
                )?);
            }
            "BUILD_ROOT" => {
                let parts = tokenize_arguments(remainder)?;
                match parts.as_slice() {
                    [value] if value == "SYSTEM" => build_root = BuildRoot::System,
                    [value] if value == "SCRATCH" => build_root = BuildRoot::Scratch,
                    [value, reference] if value == "OCI" => {
                        build_root = BuildRoot::Oci(reference.clone())
                    }
                    _ => bail!(
                        "{}:{}: BUILD_ROOT requires 'SYSTEM', 'SCRATCH', or 'OCI <ref>'",
                        path.display(),
                        line_number
                    ),
                }
            }
            "TOOLCHAIN" => {
                let value = expect_single_token(path, line_number, keyword, remainder)?;
                toolchain = match value.as_str() {
                    "SYSTEM" => ToolchainSource::System,
                    "ROOTFS" => ToolchainSource::Rootfs,
                    _ => bail!(
                        "{}:{}: TOOLCHAIN requires 'SYSTEM' or 'ROOTFS'",
                        path.display(),
                        line_number
                    ),
                };
            }
            "TOOLCHAIN_INPUT" => {
                toolchain_inputs.push(expect_single_token(path, line_number, keyword, remainder)?);
            }
            "BUILD_ARCH" => {
                build_arch = normalize_arch_name(&expect_single_token(
                    path,
                    line_number,
                    keyword,
                    remainder,
                )?)?;
            }
            "TARGET_ARCH" => {
                target_arch = normalize_arch_name(&expect_single_token(
                    path,
                    line_number,
                    keyword,
                    remainder,
                )?)?;
            }
            "TARGET_PLATFORMS" => {
                if std::mem::replace(&mut target_platforms_declared, true) {
                    bail!(
                        "{}:{}: TARGET_PLATFORMS may be declared only once",
                        path.display(),
                        line_number
                    );
                }
                let values = tokenize_arguments(remainder)?;
                if values.is_empty() {
                    bail!(
                        "{}:{}: TARGET_PLATFORMS requires at least one platform",
                        path.display(),
                        line_number
                    );
                }
                supported_target_platforms.clear();
                for value in values {
                    let platform = TargetPlatform::parse(&value).with_context(|| {
                        format!(
                            "{}:{}: invalid TARGET_PLATFORMS value",
                            path.display(),
                            line_number
                        )
                    })?;
                    if !supported_target_platforms.insert(platform) {
                        bail!(
                            "{}:{}: duplicate TARGET_PLATFORMS value '{}'",
                            path.display(),
                            line_number,
                            value
                        );
                    }
                }
            }
            "DEPENDS" => depends.push(parse_dependency_request(
                path,
                line_number,
                remainder,
                &namespace,
            )?),
            other => bail!(
                "{}:{}: unsupported DepoFile directive '{}'",
                path.display(),
                line_number,
                other
            ),
        }
    }

    if let Some(selected_build_system) = build_system {
        let configure_override;
        let build_override;
        let install_override;
        let configure_args;
        let build_args;
        let install_args;
        match selected_build_system {
            BuildSystem::Cmake => {
                ensure_phase_override_conflict(
                    path,
                    "CMAKE_CONFIGURE",
                    &cmake_configure,
                    "CMAKE_CONFIGURE_SH",
                    &cmake_configure_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "CMAKE_BUILD",
                    &cmake_build,
                    "CMAKE_BUILD_SH",
                    &cmake_build_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "CMAKE_INSTALL",
                    &cmake_install,
                    "CMAKE_INSTALL_SH",
                    &cmake_install_sh,
                )?;
                ensure_phase_structured_conflict(
                    path,
                    "CMAKE_CONFIGURE",
                    &cmake_configure,
                    &cmake_configure_sh,
                    "CMAKE_ARG/CMAKE_DEFINE",
                    !cmake_args.is_empty() || !cmake_defines.is_empty(),
                )?;
                configure_override = cmake_configure_sh;
                build_override = cmake_build_sh;
                install_override = cmake_install_sh;
                configure_args = cmake_args
                    .into_iter()
                    .chain(cmake_defines.into_iter().map(|value| format!("-D{value}")))
                    .collect::<Vec<_>>();
                build_args = Vec::new();
                install_args = Vec::new();
            }
            BuildSystem::Meson => {
                ensure_phase_override_conflict(
                    path,
                    "MESON_SETUP",
                    &meson_setup,
                    "MESON_SETUP_SH",
                    &meson_setup_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "MESON_COMPILE",
                    &meson_compile,
                    "MESON_COMPILE_SH",
                    &meson_compile_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "MESON_INSTALL",
                    &meson_install,
                    "MESON_INSTALL_SH",
                    &meson_install_sh,
                )?;
                ensure_phase_structured_conflict(
                    path,
                    "MESON_SETUP",
                    &meson_setup,
                    &meson_setup_sh,
                    "MESON_ARG/MESON_DEFINE",
                    !meson_args.is_empty() || !meson_defines.is_empty(),
                )?;
                configure_override = meson_setup_sh;
                build_override = meson_compile_sh;
                install_override = meson_install_sh;
                configure_args = meson_args
                    .into_iter()
                    .chain(meson_defines.into_iter().map(|value| format!("-D{value}")))
                    .collect::<Vec<_>>();
                build_args = Vec::new();
                install_args = Vec::new();
            }
            BuildSystem::Autoconf => {
                if autoconf_skip_configure && autoconf_configure_sh.is_some() {
                    bail!(
                        "DepoFile {} uses AUTOCONF_SKIP_CONFIGURE together with AUTOCONF_CONFIGURE_SH",
                        path.display()
                    );
                }
                if autoconf_skip_configure && autoconf_configure.is_some() {
                    bail!(
                        "DepoFile {} uses AUTOCONF_SKIP_CONFIGURE together with AUTOCONF_CONFIGURE",
                        path.display()
                    );
                }
                if autoconf_skip_configure && !autoconf_args.is_empty() {
                    bail!(
                        "DepoFile {} uses AUTOCONF_SKIP_CONFIGURE together with AUTOCONF_ARG",
                        path.display()
                    );
                }
                ensure_phase_override_conflict(
                    path,
                    "AUTOCONF_CONFIGURE",
                    &autoconf_configure,
                    "AUTOCONF_CONFIGURE_SH",
                    &autoconf_configure_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "AUTOCONF_BUILD",
                    &autoconf_build,
                    "AUTOCONF_BUILD_SH",
                    &autoconf_build_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "AUTOCONF_INSTALL",
                    &autoconf_install,
                    "AUTOCONF_INSTALL_SH",
                    &autoconf_install_sh,
                )?;
                ensure_phase_structured_conflict(
                    path,
                    "AUTOCONF_CONFIGURE",
                    &autoconf_configure,
                    &autoconf_configure_sh,
                    "AUTOCONF_ARG",
                    !autoconf_args.is_empty(),
                )?;
                configure_override = if autoconf_skip_configure {
                    Some(":".to_string())
                } else {
                    autoconf_configure_sh
                };
                build_override = autoconf_build_sh;
                install_override = autoconf_install_sh;
                configure_args = autoconf_args;
                build_args = Vec::new();
                install_args = Vec::new();
            }
            BuildSystem::Cargo => {
                ensure_phase_override_conflict(
                    path,
                    "CARGO_BUILD",
                    &cargo_build,
                    "CARGO_BUILD_SH",
                    &cargo_build_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "CARGO_INSTALL",
                    &cargo_install,
                    "CARGO_INSTALL_SH",
                    &cargo_install_sh,
                )?;
                ensure_phase_structured_conflict(
                    path,
                    "CARGO_BUILD",
                    &cargo_build,
                    &cargo_build_sh,
                    "CARGO_BUILD_ARG",
                    !cargo_build_args.is_empty(),
                )?;
                ensure_phase_structured_conflict(
                    path,
                    "CARGO_INSTALL",
                    &cargo_install,
                    &cargo_install_sh,
                    "CARGO_INSTALL_ARG",
                    !cargo_install_args.is_empty(),
                )?;
                if cargo_install_args.is_empty()
                    && cargo_install_sh.is_none()
                    && cargo_install.is_none()
                    && stage_entries.is_empty()
                {
                    bail!(
                        "DepoFile {} uses BUILD_SYSTEM CARGO but does not declare any staged outputs and does not declare CARGO_INSTALL_ARG, CARGO_INSTALL, or CARGO_INSTALL_SH",
                        path.display()
                    );
                }
                configure_override = None;
                build_override = cargo_build_sh;
                install_override = cargo_install_sh;
                configure_args = Vec::new();
                build_args = cargo_build_args;
                install_args = cargo_install_args;
            }
            BuildSystem::Manual => {
                ensure_phase_override_conflict(
                    path,
                    "MANUAL_PREPARE",
                    &manual_prepare,
                    "MANUAL_PREPARE_SH",
                    &manual_prepare_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "MANUAL_BUILD",
                    &manual_build,
                    "MANUAL_BUILD_SH",
                    &manual_build_sh,
                )?;
                ensure_phase_override_conflict(
                    path,
                    "MANUAL_INSTALL",
                    &manual_install,
                    "MANUAL_INSTALL_SH",
                    &manual_install_sh,
                )?;
                configure_override = manual_prepare_sh;
                build_override = manual_build_sh;
                install_override = manual_install_sh;
                configure_args = Vec::new();
                build_args = Vec::new();
                install_args = Vec::new();
            }
        }
        let use_default_cmake_configure = selected_build_system == BuildSystem::Cmake
            && cmake_configure.is_none()
            && configure_override.is_none();
        let commands = synthesize_build_system_commands(
            selected_build_system,
            BuildSystemCommandInputs {
                configure_args: &configure_args,
                build_args: &build_args,
                install_args: &install_args,
                configure_direct: match selected_build_system {
                    BuildSystem::Cmake => cmake_configure,
                    BuildSystem::Meson => meson_setup,
                    BuildSystem::Autoconf => autoconf_configure,
                    BuildSystem::Cargo => None,
                    BuildSystem::Manual => manual_prepare,
                },
                build_direct: match selected_build_system {
                    BuildSystem::Cmake => cmake_build,
                    BuildSystem::Meson => meson_compile,
                    BuildSystem::Autoconf => autoconf_build,
                    BuildSystem::Cargo => cargo_build,
                    BuildSystem::Manual => manual_build,
                },
                install_direct: match selected_build_system {
                    BuildSystem::Cmake => cmake_install,
                    BuildSystem::Meson => meson_install,
                    BuildSystem::Autoconf => autoconf_install,
                    BuildSystem::Cargo => cargo_install,
                    BuildSystem::Manual => manual_install,
                },
                configure_override,
                build_override,
                install_override,
            },
        );
        configure = commands.configure;
        if use_default_cmake_configure
            && matches!(
                (&build_root, &toolchain),
                (BuildRoot::Oci(_), ToolchainSource::Rootfs)
            )
            && build_arch != target_arch
        {
            if let Some(configure_command) = configure.first_mut() {
                configure_command.extend(default_cmake_cross_configure_args(&target_arch));
            }
        }
        build = commands.build;
        install = commands.install;
    }

    let name = name.context("DepoFile is missing NAME")?;
    let version = version.context("DepoFile is missing VERSION")?;
    for target in &mut targets {
        target.include_dirs = dedup_paths(std::mem::take(&mut target.include_dirs));
        if let Some(link_libraries) = pending_target_links.remove(&target.name) {
            target.link_libraries = dedup_strings(link_libraries);
        }
        if let Some(compile_definitions) = pending_target_definitions.remove(&target.name) {
            target.compile_definitions = dedup_strings(compile_definitions);
        }
        if let Some(compile_options) = pending_target_options.remove(&target.name) {
            target.compile_options = dedup_strings(compile_options);
        }
        if let Some(compile_features) = pending_target_features.remove(&target.name) {
            target.compile_features = dedup_strings(compile_features);
        }
        if !target.interface_declared
            && target.static_path.is_none()
            && target.shared_path.is_none()
            && target.object_path.is_none()
            && target.link_libraries.is_empty()
        {
            bail!(
                "DepoFile {} declares TARGET '{}' without any STATIC, SHARED, OBJECT, INTERFACE, or LINK content",
                path.display(),
                target.name
            );
        }
    }
    if let Some(primary_target) = &primary_target_name {
        if !targets.iter().any(|target| target.name == *primary_target) {
            bail!(
                "DepoFile {} declares PRIMARY_TARGET '{}' but no TARGET with that name exists",
                path.display(),
                primary_target
            );
        }
    }
    if let Some((target_name, _)) = pending_target_links.into_iter().next() {
        bail!(
            "DepoFile {} declares LINK for unknown target '{}'",
            path.display(),
            target_name
        );
    }
    if let Some((target_name, _)) = pending_target_definitions.into_iter().next() {
        bail!(
            "DepoFile {} declares DEFINES for unknown target '{}'",
            path.display(),
            target_name
        );
    }
    if let Some((target_name, _)) = pending_target_options.into_iter().next() {
        bail!(
            "DepoFile {} declares OPTIONS for unknown target '{}'",
            path.display(),
            target_name
        );
    }
    if let Some((target_name, _)) = pending_target_features.into_iter().next() {
        bail!(
            "DepoFile {} declares FEATURES for unknown target '{}'",
            path.display(),
            target_name
        );
    }
    if targets.is_empty() && artifacts.is_empty() {
        bail!(
            "DepoFile {} must declare at least one TARGET or ARTIFACT",
            path.display()
        );
    }

    let spec = PackageSpec {
        name,
        namespace,
        version,
        primary_target_name,
        source_subdir,
        lazy,
        system_libs,
        artifacts: dedup_paths(artifacts),
        targets,
        depends,
        fetch,
        git_submodules_recursive,
        configure,
        build,
        install,
        stage_entries,
        build_root,
        toolchain,
        toolchain_inputs,
        build_arch,
        target_arch,
        supported_target_platforms,
        target_platform: TargetPlatform::Host,
        build_system: build_system.unwrap_or(BuildSystem::Manual),
        origin: PackageOrigin::Local,
    };
    validate_spec_paths(path, &spec)?;
    Ok(spec)
}

fn load_catalog(depos_root: &Path) -> Result<Vec<PackageSpec>> {
    let mut catalog = builtin_catalog();
    catalog.extend(load_local_depofiles(depos_root)?);
    catalog.extend(load_embedded_depofiles(depos_root)?);
    Ok(catalog)
}

fn rebuild_embedded_depofile_catalog(depos_root: &Path, requests: &[PackageRequest]) -> Result<()> {
    let embedded_root = embedded_depofiles_root(depos_root);
    remove_existing_path(&embedded_root)?;
    if requests.is_empty() {
        return Ok(());
    }

    let mut catalog = builtin_catalog();
    catalog.extend(load_local_depofiles(depos_root)?);
    let mut hydrated = BTreeSet::new();
    for request in requests {
        hydrate_embedded_depofiles_for_request(
            depos_root,
            request,
            &mut catalog,
            &mut hydrated,
            &embedded_root,
        )?;
    }
    Ok(())
}

fn hydrate_embedded_depofiles_for_request(
    depos_root: &Path,
    request: &PackageRequest,
    catalog: &mut Vec<PackageSpec>,
    hydrated: &mut BTreeSet<String>,
    embedded_root: &Path,
) -> Result<()> {
    let by_key = group_catalog_by_key(catalog);
    let candidates = by_key.get(&request.identity_key()).with_context(|| {
        format!(
            "no registered package named '{}[{}]' while preparing embedded source DepoFiles",
            request.name, request.namespace
        )
    })?;
    let spec = select_package(candidates, &request.mode).with_context(|| {
        format!(
            "failed to resolve package '{}[{}]' while preparing embedded source DepoFiles",
            request.name, request.namespace
        )
    })?;
    hydrate_embedded_depofiles_for_spec(depos_root, &spec, catalog, hydrated, embedded_root)
}

fn hydrate_embedded_depofiles_for_spec(
    depos_root: &Path,
    spec: &PackageSpec,
    catalog: &mut Vec<PackageSpec>,
    hydrated: &mut BTreeSet<String>,
    embedded_root: &Path,
) -> Result<()> {
    if !hydrated.insert(spec.package_id()) {
        return Ok(());
    }

    if spec.fetch.is_some() {
        let mut log = String::new();
        let resolved_source = resolve_package_source(depos_root, spec, &mut log)?;
        prepare_package_source(&resolved_source.preparation, &mut log)?;
        copy_embedded_depofiles_into_catalog(
            depos_root,
            spec,
            &resolved_source.source_root,
            embedded_root,
        )?;
        *catalog = load_catalog(depos_root)?;
    }

    let by_key = group_catalog_by_key(catalog);
    for dependency in &spec.depends {
        let candidates = by_key.get(&dependency.identity_key()).with_context(|| {
            format!(
                "package '{}' depends on missing package '{}[{}]'",
                spec.package_id(),
                dependency.name,
                dependency.namespace
            )
        })?;
        let dependency_spec = select_package(candidates, &dependency.mode).with_context(|| {
            format!(
                "package '{}' could not resolve dependency '{}[{}]' while preparing embedded source DepoFiles",
                spec.package_id(),
                dependency.name,
                dependency.namespace
            )
        })?;
        hydrate_embedded_depofiles_for_spec(
            depos_root,
            &dependency_spec,
            catalog,
            hydrated,
            embedded_root,
        )?;
    }
    Ok(())
}

fn parse_registered_depofile(
    path: &Path,
    expected_name: &str,
    expected_namespace: &str,
    expected_version: &str,
) -> Result<PackageSpec> {
    ensure_package_name(expected_name)?;
    ensure_namespace_name(expected_namespace)?;
    ensure_version_name(expected_version)?;
    let mut spec = parse_depofile(path)?;
    if spec.name != expected_name {
        bail!(
            "registered DepoFile {} declares NAME {}, but is stored under {}",
            path.display(),
            spec.name,
            expected_name
        );
    }
    if spec.version != expected_version {
        bail!(
            "registered DepoFile {} declares VERSION {}, but is stored under {}",
            path.display(),
            spec.version,
            expected_version
        );
    }
    spec.namespace = expected_namespace.to_string();
    for dependency in &mut spec.depends {
        if dependency.inherit_namespace {
            dependency.namespace = spec.namespace.clone();
        }
    }
    Ok(spec)
}

fn builtin_catalog() -> Vec<PackageSpec> {
    vec![
        PackageSpec {
            name: "bitsery".to_string(),
            namespace: default_namespace(),
            version: "5.2.3".to_string(),
            primary_target_name: None,
            source_subdir: None,
            lazy: false,
            system_libs: PackageSystemLibs::Never,
            artifacts: Vec::new(),
            targets: vec![TargetSpec {
                name: "bitsery::bitsery".to_string(),
                interface_declared: true,
                include_dirs: vec![PathBuf::from("include")],
                static_path: None,
                shared_path: None,
                object_path: None,
                link_libraries: Vec::new(),
                compile_definitions: Vec::new(),
                compile_options: Vec::new(),
                compile_features: Vec::new(),
            }],
            depends: Vec::new(),
            fetch: None,
            git_submodules_recursive: false,
            configure: Vec::new(),
            build: Vec::new(),
            install: Vec::new(),
            stage_entries: Vec::new(),
            build_root: BuildRoot::System,
            toolchain: ToolchainSource::System,
            toolchain_inputs: Vec::new(),
            build_arch: host_arch(),
            target_arch: host_arch(),
            supported_target_platforms: BTreeSet::from([TargetPlatform::Host]),
            target_platform: TargetPlatform::Host,
            build_system: BuildSystem::Manual,
            origin: PackageOrigin::Builtin,
        },
        PackageSpec {
            name: "itoa".to_string(),
            namespace: default_namespace(),
            version: "main".to_string(),
            primary_target_name: None,
            source_subdir: None,
            lazy: false,
            system_libs: PackageSystemLibs::Never,
            artifacts: Vec::new(),
            targets: vec![TargetSpec {
                name: "itoa::itoa".to_string(),
                interface_declared: true,
                include_dirs: vec![PathBuf::from("include")],
                static_path: None,
                shared_path: None,
                object_path: None,
                link_libraries: Vec::new(),
                compile_definitions: Vec::new(),
                compile_options: Vec::new(),
                compile_features: Vec::new(),
            }],
            depends: Vec::new(),
            fetch: None,
            git_submodules_recursive: false,
            configure: Vec::new(),
            build: Vec::new(),
            install: Vec::new(),
            stage_entries: Vec::new(),
            build_root: BuildRoot::System,
            toolchain: ToolchainSource::System,
            toolchain_inputs: Vec::new(),
            build_arch: host_arch(),
            target_arch: host_arch(),
            supported_target_platforms: BTreeSet::from([TargetPlatform::Host]),
            target_platform: TargetPlatform::Host,
            build_system: BuildSystem::Manual,
            origin: PackageOrigin::Builtin,
        },
        PackageSpec {
            name: "zlib".to_string(),
            namespace: default_namespace(),
            version: "1.3.2".to_string(),
            primary_target_name: None,
            source_subdir: None,
            lazy: false,
            system_libs: PackageSystemLibs::Never,
            artifacts: Vec::new(),
            targets: vec![TargetSpec {
                name: "zlib::zlib".to_string(),
                interface_declared: true,
                include_dirs: vec![PathBuf::from("include")],
                static_path: Some(PathBuf::from(builtin_zlib_static_library_path())),
                shared_path: None,
                object_path: None,
                link_libraries: Vec::new(),
                compile_definitions: Vec::new(),
                compile_options: Vec::new(),
                compile_features: Vec::new(),
            }],
            depends: Vec::new(),
            fetch: None,
            git_submodules_recursive: false,
            configure: Vec::new(),
            build: Vec::new(),
            install: Vec::new(),
            stage_entries: Vec::new(),
            build_root: BuildRoot::System,
            toolchain: ToolchainSource::System,
            toolchain_inputs: Vec::new(),
            build_arch: host_arch(),
            target_arch: host_arch(),
            supported_target_platforms: BTreeSet::from([TargetPlatform::Host]),
            target_platform: TargetPlatform::Host,
            build_system: BuildSystem::Manual,
            origin: PackageOrigin::Builtin,
        },
    ]
}

fn builtin_zlib_static_library_path() -> &'static str {
    if cfg!(windows) {
        "lib/zs.lib"
    } else {
        "lib/libz.a"
    }
}

fn embedded_depofiles_root(depos_root: &Path) -> PathBuf {
    depos_root.join("depofiles").join(".embedded")
}

fn load_embedded_depofiles(depos_root: &Path) -> Result<Vec<PackageSpec>> {
    load_registered_depofiles_from_root(&embedded_depofiles_root(depos_root))
}

fn load_local_depofiles(depos_root: &Path) -> Result<Vec<PackageSpec>> {
    load_registered_depofiles_from_root(&depos_root.join("depofiles").join("local"))
}

fn load_registered_depofiles_from_root(root: &Path) -> Result<Vec<PackageSpec>> {
    let mut packages = Vec::new();
    if !root.exists() {
        return Ok(packages);
    }

    for package_entry in read_dir_sorted(&root)? {
        if !package_entry.file_type()?.is_dir() {
            continue;
        }
        let name = package_entry.file_name().to_string_lossy().into_owned();
        for namespace_entry in read_dir_sorted(&package_entry.path())? {
            if !namespace_entry.file_type()?.is_dir() {
                continue;
            }
            let namespace = namespace_entry.file_name().to_string_lossy().into_owned();
            for version_entry in read_dir_sorted(&namespace_entry.path())? {
                if !version_entry.file_type()?.is_dir() {
                    continue;
                }
                let version = version_entry.file_name().to_string_lossy().into_owned();
                let depofile_path = version_entry.path().join("main.DepoFile");
                if depofile_path.exists() {
                    packages.push(parse_registered_depofile(
                        &depofile_path,
                        &name,
                        &namespace,
                        &version,
                    )?);
                }
            }
        }
    }

    Ok(packages)
}

fn copy_embedded_depofiles_into_catalog(
    depos_root: &Path,
    owner: &PackageSpec,
    fetched_source_root: &Path,
    embedded_root: &Path,
) -> Result<()> {
    for depofiles_root in embedded_depofile_roots(owner, fetched_source_root)? {
        for depofile_path in discover_depofiles_under(&depofiles_root)? {
            let depofile_spec = parse_depofile(&depofile_path).with_context(|| {
                format!(
                    "failed to parse embedded DepoFile {} from package '{}'",
                    depofile_path.display(),
                    owner.package_id()
                )
            })?;
            if depofile_spec.name == owner.name && depofile_spec.version == owner.version {
                continue;
            }
            let local_override = registered_depofile_path(
                depos_root,
                &depofile_spec.name,
                &owner.namespace,
                &depofile_spec.version,
            );
            if local_override.exists() {
                continue;
            }
            let destination = embedded_root
                .join(&depofile_spec.name)
                .join(&owner.namespace)
                .join(&depofile_spec.version)
                .join("main.DepoFile");
            if destination.exists() {
                let existing = fs::read(&destination)
                    .with_context(|| format!("failed to read {}", destination.display()))?;
                let current = fs::read(&depofile_path)
                    .with_context(|| format!("failed to read {}", depofile_path.display()))?;
                if existing != current {
                    bail!(
                        "embedded DepoFile conflict for '{}[{}]@{}' while preparing package '{}': {} and {} differ",
                        depofile_spec.name,
                        owner.namespace,
                        depofile_spec.version,
                        owner.package_id(),
                        destination.display(),
                        depofile_path.display()
                    );
                }
                continue;
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::copy(&depofile_path, &destination).with_context(|| {
                format!(
                    "failed to copy embedded DepoFile {} to {}",
                    depofile_path.display(),
                    destination.display()
                )
            })?;
        }
    }
    Ok(())
}

fn embedded_depofile_roots(
    owner: &PackageSpec,
    fetched_source_root: &Path,
) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let source_root = resolve_source_root(fetched_source_root, owner)?;
    for candidate in [
        fetched_source_root.join("depofiles"),
        source_root.join("depofiles"),
    ] {
        if candidate.is_dir() && !roots.iter().any(|existing| existing == &candidate) {
            roots.push(candidate);
        }
    }
    Ok(roots)
}

fn discover_depofiles_under(root: &Path) -> Result<Vec<PathBuf>> {
    let mut depofiles = Vec::new();
    if !root.exists() {
        return Ok(depofiles);
    }
    discover_depofiles_under_recursive(root, &mut depofiles)?;
    depofiles.sort();
    Ok(depofiles)
}

fn discover_depofiles_under_recursive(root: &Path, depofiles: &mut Vec<PathBuf>) -> Result<()> {
    for entry in read_dir_sorted(root)? {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            discover_depofiles_under_recursive(&path, depofiles)?;
            continue;
        }
        if file_type.is_file() && path.extension() == Some(OsStr::new("DepoFile")) {
            depofiles.push(path);
        }
    }
    Ok(())
}

fn resolve_requests(
    catalog: &[PackageSpec],
    requests: &[PackageRequest],
) -> Result<Vec<ResolvedPackage>> {
    let by_key = group_catalog_by_key(catalog);
    let mut resolved = BTreeMap::<PackageKey, ResolvedPackage>::new();
    for request in requests {
        resolve_request_recursive(request, &by_key, &mut resolved)?;
    }
    topologically_order_resolved(&resolved)
}

fn resolve_request_recursive(
    request: &PackageRequest,
    by_key: &BTreeMap<PackageKey, Vec<PackageSpec>>,
    resolved: &mut BTreeMap<PackageKey, ResolvedPackage>,
) -> Result<()> {
    let key = request.identity_key();
    if let Some(existing) = resolved.get_mut(&key) {
        ensure_request_compatible(existing, request)?;
        if request.alias.is_none() {
            existing.expose_default = true;
        }
        if let Some(alias) = &request.alias {
            existing.aliases.insert(alias.clone());
        }
        return Ok(());
    }

    if request.source == RequestSource::System {
        bail!(
            "request '{}[{}]' asked for SOURCE SYSTEM, but the reconstructed depos core currently supports only materialized Depo artifacts",
            request.name,
            request.namespace
        );
    }

    let candidates = by_key.get(&key).with_context(|| {
        format!(
            "no registered package named '{}[{}]'",
            request.name, request.namespace
        )
    })?;
    let spec = select_package(candidates, &request.mode).with_context(|| {
        format!(
            "failed to resolve package '{}[{}]'",
            request.name, request.namespace
        )
    })?;

    let resolved_package = ResolvedPackage {
        spec: spec.clone(),
        source: request.source.clone(),
        request: request.mode.clone(),
        expose_default: request.alias.is_none(),
        aliases: request.alias.iter().cloned().collect(),
    };
    resolved.insert(key, resolved_package.clone());

    for dependency in &spec.depends {
        resolve_request_recursive(dependency, by_key, resolved)?;
    }

    Ok(())
}

fn ensure_request_compatible(existing: &ResolvedPackage, request: &PackageRequest) -> Result<()> {
    if existing.spec.name != request.name || existing.spec.namespace != request.namespace {
        bail!(
            "internal resolver mismatch for '{}[{}]'",
            request.name,
            request.namespace
        );
    }

    let request_matches = match &request.mode {
        RequestMode::Latest => true,
        RequestMode::Exact(version) => &existing.spec.version == version,
        RequestMode::Minimum(version) => {
            compare_versions(&existing.spec.version, version) != Ordering::Less
        }
    };
    if !request_matches {
        bail!(
            "package '{}[{}]' was already resolved to '{}', which does not satisfy the later request '{} {}'",
            request.name,
            request.namespace,
            existing.spec.version,
            request.mode.kind_str(),
            request.mode.version_str()
        );
    }
    if request.source == RequestSource::System {
        bail!(
            "package '{}[{}]' requested SOURCE SYSTEM after already resolving to a Depo artifact",
            request.name,
            request.namespace
        );
    }
    Ok(())
}

fn topologically_order_resolved(
    resolved: &BTreeMap<PackageKey, ResolvedPackage>,
) -> Result<Vec<ResolvedPackage>> {
    let mut ordered = Vec::with_capacity(resolved.len());
    let mut permanent = BTreeSet::<PackageKey>::new();
    let mut visiting = BTreeSet::<PackageKey>::new();
    for key in resolved.keys() {
        visit_resolved_package(key, resolved, &mut permanent, &mut visiting, &mut ordered)?;
    }
    Ok(ordered)
}

fn visit_resolved_package(
    key: &PackageKey,
    resolved: &BTreeMap<PackageKey, ResolvedPackage>,
    permanent: &mut BTreeSet<PackageKey>,
    visiting: &mut BTreeSet<PackageKey>,
    ordered: &mut Vec<ResolvedPackage>,
) -> Result<()> {
    if permanent.contains(key) {
        return Ok(());
    }
    if !visiting.insert(key.clone()) {
        bail!(
            "dependency cycle detected while ordering '{}[{}]'",
            key.name,
            key.namespace
        );
    }
    let package = resolved.get(key).with_context(|| {
        format!(
            "internal resolver mismatch while ordering '{}[{}]'",
            key.name, key.namespace
        )
    })?;
    for dependency in &package.spec.depends {
        let dependency_key = dependency.identity_key();
        if !resolved.contains_key(&dependency_key) {
            bail!(
                "internal resolver mismatch: '{}[{}]' depends on unresolved '{}[{}]'",
                package.spec.name,
                package.spec.namespace,
                dependency.name,
                dependency.namespace
            );
        }
        visit_resolved_package(&dependency_key, resolved, permanent, visiting, ordered)?;
    }
    visiting.remove(key);
    permanent.insert(key.clone());
    ordered.push(package.clone());
    Ok(())
}

fn select_package(candidates: &[PackageSpec], mode: &RequestMode) -> Result<PackageSpec> {
    let mut matches = candidates
        .iter()
        .filter(|candidate| match mode {
            RequestMode::Latest => true,
            RequestMode::Exact(version) => candidate.version == *version,
            RequestMode::Minimum(version) => {
                compare_versions(&candidate.version, version) != Ordering::Less
            }
        })
        .cloned()
        .collect::<Vec<_>>();

    if matches.is_empty() {
        match mode {
            RequestMode::Latest => bail!("no versions are registered"),
            RequestMode::Exact(version) => bail!("exact version '{}' is not registered", version),
            RequestMode::Minimum(version) => bail!("no version satisfies minimum '{}'", version),
        }
    }

    matches.sort_by(|left, right| compare_versions(&left.version, &right.version));
    Ok(matches.pop().expect("matches is not empty"))
}

fn validate_materialized_packages(
    depos_root: &Path,
    variant: &str,
    selected: &[ResolvedPackage],
) -> Result<()> {
    for package in selected {
        let store_root = package_store_root_for_selected(depos_root, variant, package);
        for relative_path in package.spec.required_paths() {
            let absolute_path = store_root.join(&relative_path);
            if !path_exists_or_symlink(&absolute_path)? {
                bail!(
                    "package '{}' is selected but required path '{}' is missing under '{}'",
                    package.spec.package_id(),
                    relative_path.display(),
                    store_root.display()
                );
            }
        }
    }
    Ok(())
}

fn render_validate_cmake(depos_root: &Path, variant: &str, selected: &[ResolvedPackage]) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "set(DEPOS_VALIDATE_VARIANT_ROOT \"{}\")\n",
        cmake_escape(&display_path(&depos_root.join("store").join(variant)))
    ));
    output.push_str("function(_depos_validate_required_path package_id absolute_path)\n");
    output.push_str("  set(_depos_abs_path \"${absolute_path}\")\n");
    output.push_str("  if (EXISTS \"${_depos_abs_path}\" OR IS_SYMLINK \"${_depos_abs_path}\")\n");
    output.push_str("    return()\n");
    output.push_str("  endif()\n\n");
    output.push_str("  message(FATAL_ERROR \"Depo sync validation failed for '${package_id}': missing required path '${absolute_path}'.\")\n");
    output.push_str("endfunction()\n");
    for package in selected {
        let package_root = package_store_root_for_selected(depos_root, variant, package);
        for path in package.spec.required_paths() {
            output.push_str(&format!(
                "_depos_validate_required_path(\"{}\" \"{}\")\n",
                cmake_escape(&package.spec.package_id()),
                cmake_escape(&display_path(&package_root.join(path)))
            ));
        }
    }
    output
}

fn sanitize_cmake_identifier(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn internal_target_name(package: &ResolvedPackage, target_index: usize) -> String {
    format!(
        "_depos_{}_{}_{}_t{}",
        sanitize_cmake_identifier(&package.spec.name),
        sanitize_cmake_identifier(&package.spec.namespace),
        sanitize_cmake_identifier(&package.spec.version),
        target_index
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetPublicRole {
    Base,
    Interface,
    Static,
    Shared,
    Object,
}

impl TargetPublicRole {
    fn public_suffix(self) -> Option<&'static str> {
        match self {
            Self::Base => None,
            Self::Interface => Some("interface"),
            Self::Static => Some("static"),
            Self::Shared => Some("shared"),
            Self::Object => Some("object"),
        }
    }

    fn internal_suffix(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Interface => "interface",
            Self::Static => "static",
            Self::Shared => "shared",
            Self::Object => "object",
        }
    }
}

fn internal_target_component_name(
    package: &ResolvedPackage,
    target_index: usize,
    role: TargetPublicRole,
) -> String {
    if role == TargetPublicRole::Base {
        internal_target_name(package, target_index)
    } else {
        format!(
            "{}__{}",
            internal_target_name(package, target_index),
            role.internal_suffix()
        )
    }
}

fn public_target_component_name(name: &str, role: TargetPublicRole) -> String {
    match role.public_suffix() {
        Some(suffix) => format!("{name}::{suffix}"),
        None => name.to_string(),
    }
}

fn alias_target_name(alias: &str, original_target_name: &str) -> String {
    if let Some((_, suffix)) = original_target_name.split_once("::") {
        format!("{alias}::{suffix}")
    } else {
        alias.to_string()
    }
}

fn public_target_bindings(
    package: &ResolvedPackage,
    target: &TargetSpec,
    target_index: usize,
) -> Vec<(String, String)> {
    let mut roots = BTreeSet::new();
    if package.expose_default {
        roots.insert(target.name.clone());
    }
    for alias in &package.aliases {
        roots.insert(alias_target_name(alias, &target.name));
    }

    let mut bindings = Vec::new();
    for root in roots {
        bindings.push((
            public_target_component_name(&root, TargetPublicRole::Base),
            internal_target_component_name(package, target_index, TargetPublicRole::Base),
        ));
        bindings.push((
            public_target_component_name(&root, TargetPublicRole::Interface),
            internal_target_component_name(package, target_index, TargetPublicRole::Interface),
        ));
        if target.static_path.is_some() {
            bindings.push((
                public_target_component_name(&root, TargetPublicRole::Static),
                internal_target_component_name(package, target_index, TargetPublicRole::Static),
            ));
        }
        if target.shared_path.is_some() {
            bindings.push((
                public_target_component_name(&root, TargetPublicRole::Shared),
                internal_target_component_name(package, target_index, TargetPublicRole::Shared),
            ));
        }
        if target.object_path.is_some() {
            bindings.push((
                public_target_component_name(&root, TargetPublicRole::Object),
                internal_target_component_name(package, target_index, TargetPublicRole::Object),
            ));
        }
    }
    bindings
}

fn ensure_public_target_collisions(selected: &[ResolvedPackage]) -> Result<()> {
    let mut owners = BTreeMap::<String, String>::new();
    for package in selected {
        for (target_index, target) in package.spec.targets.iter().enumerate() {
            for (public_name, _) in public_target_bindings(package, target, target_index) {
                if let Some(existing) =
                    owners.insert(public_name.clone(), package.spec.package_id())
                {
                    bail!(
                        "public target '{}' is exported by both '{}' and '{}'; use manifest AS aliasing to disambiguate",
                        public_name,
                        existing,
                        package.spec.package_id()
                    );
                }
            }
        }
    }
    Ok(())
}

fn global_target_lookup(selected: &[ResolvedPackage]) -> BTreeMap<String, Option<String>> {
    let mut lookup = BTreeMap::<String, Option<String>>::new();
    for package in selected {
        for (target_index, target) in package.spec.targets.iter().enumerate() {
            for (public_name, internal_name) in
                public_target_bindings(package, target, target_index)
            {
                match lookup.get(&public_name) {
                    Some(Some(existing)) if existing != &internal_name => {
                        lookup.insert(public_name.clone(), None);
                    }
                    Some(None) => {}
                    _ => {
                        lookup.insert(public_name, Some(internal_name));
                    }
                }
            }
        }
    }
    lookup
}

fn render_targets_cmake(
    depos_root: &Path,
    variant: &str,
    validate_file: &Path,
    selected: &[ResolvedPackage],
) -> Result<String> {
    let dependency_links = dependency_primary_targets(selected);
    let target_lookup = global_target_lookup(selected);
    ensure_public_target_collisions(selected)?;
    let mut output = String::new();
    output.push_str("include_guard(GLOBAL)\n");
    output.push_str(&format!(
        "set(DEPOS_REGISTRY_STORE_ROOT \"{}\")\n",
        cmake_escape(&display_path(&depos_root.join("store").join(variant)))
    ));
    output.push_str(&format!(
        "include(\"{}\")\n",
        cmake_escape(&display_path(validate_file))
    ));

    for package in selected {
        output.push_str(&format!("\n# {}\n", package.spec.package_id()));
        let package_root = package_store_root_for_selected(depos_root, variant, package);
        let dependency_targets = package
            .spec
            .depends
            .iter()
            .filter_map(|dependency| dependency_links.get(&dependency.identity_key()))
            .cloned()
            .collect::<Vec<_>>();
        let mut package_targets = BTreeMap::new();
        for (index, target) in package.spec.targets.iter().enumerate() {
            package_targets.insert(
                target.name.clone(),
                internal_target_component_name(package, index, TargetPublicRole::Base),
            );
            package_targets.insert(
                public_target_component_name(&target.name, TargetPublicRole::Interface),
                internal_target_component_name(package, index, TargetPublicRole::Interface),
            );
            if target.static_path.is_some() {
                package_targets.insert(
                    public_target_component_name(&target.name, TargetPublicRole::Static),
                    internal_target_component_name(package, index, TargetPublicRole::Static),
                );
            }
            if target.shared_path.is_some() {
                package_targets.insert(
                    public_target_component_name(&target.name, TargetPublicRole::Shared),
                    internal_target_component_name(package, index, TargetPublicRole::Shared),
                );
            }
            if target.object_path.is_some() {
                package_targets.insert(
                    public_target_component_name(&target.name, TargetPublicRole::Object),
                    internal_target_component_name(package, index, TargetPublicRole::Object),
                );
            }
        }

        for (target_index, target) in package.spec.targets.iter().enumerate() {
            let interface_internal =
                internal_target_component_name(package, target_index, TargetPublicRole::Interface);
            let base_internal =
                internal_target_component_name(package, target_index, TargetPublicRole::Base);
            let include_dirs = target
                .include_dirs
                .iter()
                .map(|path| display_path(&package_root.join(path)))
                .collect::<Vec<_>>();
            let mut interface_links = dependency_targets.clone();
            interface_links.extend(target.link_libraries.iter().map(|item| {
                if let Some(internal_name) = package_targets.get(item) {
                    internal_name.clone()
                } else if let Some(Some(internal_name)) = target_lookup.get(item) {
                    internal_name.clone()
                } else {
                    item.clone()
                }
            }));
            interface_links = dedup_strings(interface_links);

            output.push_str(&format!("if (NOT TARGET {})\n", interface_internal));
            output.push_str(&format!(
                "  add_library({} INTERFACE IMPORTED GLOBAL)\n",
                interface_internal
            ));
            let mut interface_properties = Vec::new();
            if !include_dirs.is_empty() {
                interface_properties.push(format!(
                    "INTERFACE_INCLUDE_DIRECTORIES \"{}\"",
                    cmake_escape(&include_dirs.join(";"))
                ));
            }
            if !interface_links.is_empty() {
                interface_properties.push(format!(
                    "INTERFACE_LINK_LIBRARIES \"{}\"",
                    cmake_escape(&interface_links.join(";"))
                ));
            }
            if !target.compile_definitions.is_empty() {
                interface_properties.push(format!(
                    "INTERFACE_COMPILE_DEFINITIONS \"{}\"",
                    cmake_escape(&target.compile_definitions.join(";"))
                ));
            }
            if !target.compile_options.is_empty() {
                interface_properties.push(format!(
                    "INTERFACE_COMPILE_OPTIONS \"{}\"",
                    cmake_escape(&target.compile_options.join(";"))
                ));
            }
            if !target.compile_features.is_empty() {
                interface_properties.push(format!(
                    "INTERFACE_COMPILE_FEATURES \"{}\"",
                    cmake_escape(&target.compile_features.join(";"))
                ));
            }
            if !interface_properties.is_empty() {
                output.push_str(&format!(
                    "  set_target_properties({} PROPERTIES\n",
                    interface_internal
                ));
                for property in interface_properties {
                    output.push_str(&format!("    {}\n", property));
                }
                output.push_str("  )\n");
            }
            output.push_str("endif()\n");

            let mut artifact_targets = Vec::new();
            for (role, kind, path) in [
                (
                    TargetPublicRole::Static,
                    TargetKind::Static,
                    target.static_path.as_ref(),
                ),
                (
                    TargetPublicRole::Shared,
                    TargetKind::Shared,
                    target.shared_path.as_ref(),
                ),
                (
                    TargetPublicRole::Object,
                    TargetKind::Object,
                    target.object_path.as_ref(),
                ),
            ] {
                let Some(path) = path else {
                    continue;
                };
                let artifact_internal = internal_target_component_name(package, target_index, role);
                artifact_targets.push(artifact_internal.clone());
                output.push_str(&format!("if (NOT TARGET {})\n", artifact_internal));
                output.push_str(&format!(
                    "  add_library({} {} IMPORTED GLOBAL)\n",
                    artifact_internal,
                    kind.cmake_imported_type()
                ));
                let abs_path = display_path(&package_root.join(path));
                let property_name = match kind {
                    TargetKind::Object => "IMPORTED_OBJECTS",
                    _ => "IMPORTED_LOCATION",
                };
                output.push_str(&format!(
                    "  set_target_properties({} PROPERTIES\n",
                    artifact_internal
                ));
                output.push_str(&format!(
                    "    {} \"{}\"\n",
                    property_name,
                    cmake_escape(&abs_path)
                ));
                output.push_str(&format!(
                    "    INTERFACE_LINK_LIBRARIES \"{}\"\n",
                    cmake_escape(&interface_internal)
                ));
                output.push_str("  )\n");
                output.push_str("endif()\n");
            }

            output.push_str(&format!("if (NOT TARGET {})\n", base_internal));
            output.push_str(&format!(
                "  add_library({} INTERFACE IMPORTED GLOBAL)\n",
                base_internal
            ));
            let base_links = match artifact_targets.as_slice() {
                [] => vec![interface_internal.clone()],
                [only] => vec![only.clone()],
                _ => vec![interface_internal.clone()],
            };
            if !base_links.is_empty() {
                output.push_str(&format!(
                    "  set_target_properties({} PROPERTIES\n",
                    base_internal
                ));
                output.push_str(&format!(
                    "    INTERFACE_LINK_LIBRARIES \"{}\"\n",
                    cmake_escape(&base_links.join(";"))
                ));
                output.push_str("  )\n");
            }
            output.push_str("endif()\n");

            for (public_name, internal_name) in
                public_target_bindings(package, target, target_index)
            {
                output.push_str(&format!("if (NOT TARGET {})\n", public_name));
                output.push_str(&format!(
                    "  add_library({} ALIAS {})\n",
                    public_name, internal_name
                ));
                output.push_str("endif()\n");
            }
        }
    }

    Ok(output)
}

fn render_lock_cmake(
    depos_root: &Path,
    manifest: &Path,
    variant: &str,
    registry_dir: &Path,
    selected: &[ResolvedPackage],
) -> String {
    let targets_file = registry_dir.join("targets.cmake");
    let validate_file = registry_dir.join("validate.cmake");
    let profile = registry_dir
        .file_name()
        .unwrap_or_else(|| OsStr::new(""))
        .to_string_lossy()
        .into_owned();
    let selected_packages = selected
        .iter()
        .map(|package| {
            let aliases = if package.aliases.is_empty() {
                "_".to_string()
            } else {
                package
                    .aliases
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let provenance = read_source_provenance(
                depos_root,
                &package.spec.name,
                &package.spec.namespace,
                &package.spec.version,
            )
            .unwrap_or_default();
            format!(
                "{}|{}|{}|{}|{}|{}|{}",
                package.spec.package_id(),
                package.source.as_str(),
                package.request.kind_str(),
                if package.request.version_str().is_empty() {
                    "_".to_string()
                } else {
                    package.request.version_str().to_string()
                },
                aliases,
                provenance.source_ref.unwrap_or_else(|| "_".to_string()),
                provenance.source_commit.unwrap_or_else(|| "_".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join(";");

    let mut output = String::new();
    output.push_str(&format!(
        "set(DEPOS_REGISTRY_MANIFEST \"{}\")\n",
        cmake_escape(&display_path(manifest))
    ));
    output.push_str(&format!(
        "set(DEPOS_REGISTRY_PROFILE \"{}\")\n",
        cmake_escape(&profile)
    ));
    output.push_str(&format!(
        "set(DEPOS_REGISTRY_VARIANT \"{}\")\n",
        cmake_escape(variant)
    ));
    output.push_str(&format!(
        "set(DEPOS_REGISTRY_STORE_ROOT \"{}\")\n",
        cmake_escape(&display_path(&depos_root.join("store").join(variant)))
    ));
    output.push_str(&format!(
        "set(DEPOS_REGISTRY_TARGETS_FILE \"{}\")\n",
        cmake_escape(&display_path(&targets_file))
    ));
    output.push_str(&format!(
        "set(DEPOS_REGISTRY_VALIDATE_FILE \"{}\")\n",
        cmake_escape(&display_path(&validate_file))
    ));
    output.push_str(&format!(
        "set(DEPOS_REGISTRY_SELECTED_PACKAGES \"{}\")\n",
        cmake_escape(&selected_packages)
    ));
    output
}

fn dependency_primary_targets(selected: &[ResolvedPackage]) -> BTreeMap<PackageKey, String> {
    let mut map = BTreeMap::new();
    for package in selected {
        if package.spec.primary_target().is_some() {
            let primary_index = package
                .spec
                .primary_target_index()
                .expect("primary_target checked");
            map.insert(
                package.spec.identity_key(),
                internal_target_name(package, primary_index),
            );
        }
    }
    map
}

fn refresh_status(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
) -> Result<PackageStatus> {
    refresh_status_for_target_platform(depos_root, name, namespace, version, TargetPlatform::Host)
}

fn refresh_status_for_target_platform(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
    target_platform: TargetPlatform,
) -> Result<PackageStatus> {
    let depofile_path = resolve_registered_depofile_path(depos_root, name, namespace, version)?;
    let mut spec = parse_registered_depofile(&depofile_path, name, namespace, version)?;
    spec.target_platform = target_platform;
    let catalog = load_catalog(depos_root)?;
    let missing_dependencies = spec
        .depends
        .iter()
        .filter_map(|dependency| dependency_missing_reason(&catalog, dependency))
        .collect::<Vec<_>>();

    let variant = variant_for_target(&spec.target_arch, spec.target_platform)?;
    let store_root = package_store_root(depos_root, &variant, &spec);
    let provenance =
        read_source_provenance(depos_root, &spec.name, &spec.namespace, &spec.version)?;

    let (state, message) = if !missing_dependencies.is_empty() {
        (
            PackageState::Quarantined,
            format!(
                "missing Depo dependencies: {}",
                missing_dependencies.join(", ")
            ),
        )
    } else if spec
        .required_paths()
        .iter()
        .all(|path| path_exists_or_symlink(&store_root.join(path)).unwrap_or(false))
    {
        (
            PackageState::Green,
            format!(
                "all declared exports are present under {}",
                store_root.display()
            ),
        )
    } else {
        (
            PackageState::NeverRun,
            format!(
                "registered, but declared exports are not present under {}",
                store_root.display()
            ),
        )
    };

    let status = PackageStatus {
        name: spec.name.clone(),
        namespace: spec.namespace.clone(),
        version: spec.version.clone(),
        lazy: spec.lazy,
        system_libs: spec.system_libs.clone(),
        state,
        depofile: depofile_path.clone(),
        message,
        source_ref: provenance.source_ref,
        source_commit: provenance.source_commit,
    };
    write_status_file(depos_root, &status, &variant)?;
    Ok(status)
}

fn dependency_missing_reason(
    catalog: &[PackageSpec],
    dependency: &PackageRequest,
) -> Option<String> {
    let key = dependency.identity_key();
    let matches = catalog
        .iter()
        .filter(|candidate| candidate.identity_key() == key)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Some(format!("{}[{}]", dependency.name, dependency.namespace));
    }
    let resolved = select_package(
        &matches.into_iter().cloned().collect::<Vec<_>>(),
        &dependency.mode,
    );
    if resolved.is_err() {
        return Some(format!(
            "{}[{}] ({})",
            dependency.name,
            dependency.namespace,
            dependency.mode.version_str()
        ));
    }
    None
}

fn read_or_refresh_status_for_target_platform(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
    target_platform: TargetPlatform,
) -> Result<PackageStatus> {
    let depofile = resolve_registered_depofile_path(depos_root, name, namespace, version)?;
    let spec = parse_registered_depofile(&depofile, name, namespace, version)?;
    let variant = variant_for_target(&spec.target_arch, target_platform)?;
    let path = status_file_path(depos_root, name, namespace, version, &variant);
    if path.exists() {
        read_status_file(&path)
    } else {
        refresh_status_for_target_platform(depos_root, name, namespace, version, target_platform)
    }
}

fn write_status_file(depos_root: &Path, status: &PackageStatus, variant: &str) -> Result<()> {
    let path = status_file_path(
        depos_root,
        &status.name,
        &status.namespace,
        &status.version,
        variant,
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid status file path {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut content = format!(
        "name={}\nnamespace={}\nversion={}\nlazy={}\nsystem_libs={}\nstate={}\ndepofile={}\nmessage={}\n",
        status.name,
        status.namespace,
        status.version,
        if status.lazy { "true" } else { "false" },
        status.system_libs.as_str(),
        status.state.as_str(),
        display_path(&status.depofile),
        status.message
    );
    if let Some(source_ref) = &status.source_ref {
        content.push_str("source_ref=");
        content.push_str(source_ref);
        content.push('\n');
    }
    if let Some(source_commit) = &status.source_commit {
        content.push_str("source_commit=");
        content.push_str(source_commit);
        content.push('\n');
    }
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn read_status_file(path: &Path) -> Result<PackageStatus> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut values = BTreeMap::<String, String>::new();
    for line in source.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid status line '{}'", line))?;
        values.insert(key.to_string(), value.to_string());
    }
    Ok(PackageStatus {
        name: values
            .remove("name")
            .context("status file is missing name")?,
        namespace: values.remove("namespace").unwrap_or_else(default_namespace),
        version: values
            .remove("version")
            .context("status file is missing version")?,
        lazy: values
            .remove("lazy")
            .context("status file is missing lazy")?
            == "true",
        system_libs: PackageSystemLibs::parse(
            &values
                .remove("system_libs")
                .context("status file is missing system_libs")?,
        )?,
        state: PackageState::parse(
            &values
                .remove("state")
                .context("status file is missing state")?,
        )?,
        depofile: PathBuf::from(
            values
                .remove("depofile")
                .context("status file is missing depofile")?,
        ),
        message: values
            .remove("message")
            .context("status file is missing message")?,
        source_ref: values.remove("source_ref"),
        source_commit: values.remove("source_commit"),
    })
}

fn registered_packages(depos_root: &Path) -> Result<Vec<(String, String, String)>> {
    let root = depos_root.join("depofiles").join("local");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut packages = Vec::new();
    for name_entry in read_dir_sorted(&root)? {
        if !name_entry.file_type()?.is_dir() {
            continue;
        }
        let name = name_entry.file_name().to_string_lossy().into_owned();
        for namespace_entry in read_dir_sorted(&name_entry.path())? {
            if !namespace_entry.file_type()?.is_dir() {
                continue;
            }
            let namespace = namespace_entry.file_name().to_string_lossy().into_owned();
            for version_entry in read_dir_sorted(&namespace_entry.path())? {
                if !version_entry.file_type()?.is_dir() {
                    continue;
                }
                let version = version_entry.file_name().to_string_lossy().into_owned();
                if version_entry.path().join("main.DepoFile").exists() {
                    packages.push((name.clone(), namespace.clone(), version));
                }
            }
        }
    }
    Ok(packages)
}

fn resolve_depos_root(path: &Path) -> Result<PathBuf> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    canonical_path(path)
}

fn resolve_depos_executable(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return canonical_path(path);
    }
    std::env::current_exe().context("failed to locate depos executable")
}

fn manifest_profile(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let hash = Sha256::digest(bytes);
    Ok(format!("manifest-{:x}", hash)[..25].to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DepofileDirective {
    line_number: usize,
    keyword: String,
    remainder: String,
    block: Option<String>,
}

fn depofile_directives(path: &Path, source: &str) -> Result<Vec<DepofileDirective>> {
    let mut directives = Vec::new();
    let lines = source.lines().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < lines.len() {
        let line_number = index + 1;
        let raw_line = lines[index];
        index += 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (keyword, remainder) = split_keyword(trimmed).ok_or_else(|| {
            anyhow!(
                "invalid DepoFile syntax at {}:{}",
                path.display(),
                line_number
            )
        })?;
        if let Some(raw_delimiter) = remainder.strip_prefix("<<") {
            let delimiter = parse_block_delimiter(path, line_number, keyword, raw_delimiter)?;
            let mut body = String::new();
            let mut terminated = false;
            while index < lines.len() {
                let candidate = lines[index];
                index += 1;
                if candidate.trim() == delimiter {
                    terminated = true;
                    break;
                }
                body.push_str(candidate);
                body.push('\n');
            }
            if !terminated {
                bail!(
                    "{}:{}: unterminated {} block; missing terminator {}",
                    path.display(),
                    line_number,
                    keyword,
                    delimiter
                );
            }
            directives.push(DepofileDirective {
                line_number,
                keyword: keyword.to_string(),
                remainder: String::new(),
                block: Some(body),
            });
            continue;
        }
        directives.push(DepofileDirective {
            line_number,
            keyword: keyword.to_string(),
            remainder: remainder.to_string(),
            block: None,
        });
    }
    Ok(directives)
}

fn parse_block_delimiter(
    path: &Path,
    line_number: usize,
    keyword: &str,
    raw_value: &str,
) -> Result<String> {
    let delimiter = raw_value.trim();
    if delimiter.is_empty() {
        bail!(
            "{}:{}: {} block requires a heredoc delimiter after <<",
            path.display(),
            line_number,
            keyword
        );
    }
    if let Some(value) = delimiter
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        if value.is_empty() {
            bail!(
                "{}:{}: {} block delimiter must not be empty",
                path.display(),
                line_number,
                keyword
            );
        }
        return Ok(value.to_string());
    }
    if let Some(value) = delimiter
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        if value.is_empty() {
            bail!(
                "{}:{}: {} block delimiter must not be empty",
                path.display(),
                line_number,
                keyword
            );
        }
        return Ok(value.to_string());
    }
    Ok(delimiter.to_string())
}

fn canonical_path(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("failed to canonicalize {}", path.display()))
}

#[cfg(target_os = "windows")]
fn normalize_host_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(stripped) = raw.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{stripped}"));
    }
    if let Some(stripped) = raw.strip_prefix(r"\\?\") {
        return PathBuf::from(stripped);
    }
    path.to_path_buf()
}

#[cfg(not(target_os = "windows"))]
fn normalize_host_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn ensure_relative_confined_path(path: &Path, context: &str) -> Result<()> {
    if path.is_absolute() {
        bail!("{context} must be relative, got '{}'", path.display());
    }
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                bail!("{context} must be relative, got '{}'", path.display())
            }
            Component::ParentDir => {
                bail!("{context} must not contain '..', got '{}'", path.display())
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn ensure_archive_member_path_safe(member: &Path, archive_path: &Path) -> Result<()> {
    if member.as_os_str().is_empty() {
        bail!(
            "archive '{}' contains an empty entry name",
            archive_path.display()
        );
    }
    ensure_relative_confined_path(
        member,
        &format!(
            "archive '{}' entry '{}'",
            archive_path.display(),
            member.display()
        ),
    )
}

fn path_exists_or_symlink(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn cmake_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn display_path(path: &Path) -> String {
    normalize_host_path(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for path in paths {
        if seen.insert(path.clone()) {
            output.push(path);
        }
    }
    output
}

fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if let Some(index) = trimmed.find(char::is_whitespace) {
        Some((&trimmed[..index], trimmed[index..].trim()))
    } else {
        Some((trimmed, ""))
    }
}

fn ensure_empty(path: &Path, line_number: usize, keyword: &str, remainder: &str) -> Result<()> {
    if !remainder.is_empty() {
        bail!(
            "{}:{}: {} does not accept arguments",
            path.display(),
            line_number,
            keyword
        );
    }
    Ok(())
}

fn expect_single_token(
    path: &Path,
    line_number: usize,
    keyword: &str,
    remainder: &str,
) -> Result<String> {
    let tokens = tokenize_arguments(remainder)?;
    if tokens.len() != 1 {
        bail!(
            "{}:{}: {} requires exactly one value",
            path.display(),
            line_number,
            keyword
        );
    }
    Ok(tokens[0].clone())
}

fn block_body(
    path: &Path,
    line_number: usize,
    keyword: &str,
    block: Option<&String>,
) -> Result<String> {
    block.cloned().with_context(|| {
        format!(
            "{}:{}: {} requires a heredoc block",
            path.display(),
            line_number,
            keyword
        )
    })
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'/' | b'.' | b'_' | b'-' | b':' | b'=' | b'+' | b'@' | b'%'
            )
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn shell_join(values: &[String]) -> String {
    values
        .iter()
        .map(|value| shell_quote(value))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_phase(script: String) -> Vec<Vec<String>> {
    vec![vec![
        "sh".to_string(),
        "-eu".to_string(),
        "-c".to_string(),
        script,
    ]]
}

fn parse_stage_entry(
    path: &Path,
    line_number: usize,
    remainder: &str,
    kind: StageKind,
) -> Result<StageEntry> {
    let parts = tokenize_arguments(remainder)?;
    match parts.as_slice() {
        [root, source, destination] => Ok(StageEntry {
            kind,
            source_root: parse_stage_source_root(path, line_number, root)?,
            source: PathBuf::from(source),
            destination: PathBuf::from(destination),
        }),
        [root, source] => Ok(StageEntry {
            kind,
            source_root: parse_stage_source_root(path, line_number, root)?,
            source: PathBuf::from(source),
            destination: PathBuf::from(source),
        }),
        _ => bail!(
            "{}:{}: {} requires '<SOURCE|BUILD> <source> [destination]'",
            path.display(),
            line_number,
            match kind {
                StageKind::File => "STAGE_FILE",
                StageKind::Tree => "STAGE_TREE",
            }
        ),
    }
}

fn parse_stage_source_root(
    path: &Path,
    line_number: usize,
    value: &str,
) -> Result<StageSourceRoot> {
    match value {
        "SOURCE" => Ok(StageSourceRoot::Source),
        "BUILD" => Ok(StageSourceRoot::Build),
        _ => bail!(
            "{}:{}: stage root must be SOURCE or BUILD, got '{}'",
            path.display(),
            line_number,
            value
        ),
    }
}

fn validate_target_paths(path: &Path, target: &TargetSpec) -> Result<()> {
    for include_dir in &target.include_dirs {
        ensure_relative_confined_path(
            include_dir,
            &format!(
                "DepoFile {} TARGET {} INTERFACE path",
                path.display(),
                target.name
            ),
        )?;
    }
    if let Some(static_path) = &target.static_path {
        ensure_relative_confined_path(
            static_path,
            &format!(
                "DepoFile {} TARGET {} STATIC path",
                path.display(),
                target.name
            ),
        )?;
    }
    if let Some(shared_path) = &target.shared_path {
        ensure_relative_confined_path(
            shared_path,
            &format!(
                "DepoFile {} TARGET {} SHARED path",
                path.display(),
                target.name
            ),
        )?;
    }
    if let Some(object_path) = &target.object_path {
        ensure_relative_confined_path(
            object_path,
            &format!(
                "DepoFile {} TARGET {} OBJECT path",
                path.display(),
                target.name
            ),
        )?;
    }
    Ok(())
}

fn validate_spec_paths(path: &Path, spec: &PackageSpec) -> Result<()> {
    if let Some(source_subdir) = &spec.source_subdir {
        ensure_relative_confined_path(
            source_subdir,
            &format!("DepoFile {} SOURCE_SUBDIR", path.display()),
        )?;
    }
    for artifact in &spec.artifacts {
        ensure_relative_confined_path(artifact, &format!("DepoFile {} ARTIFACT", path.display()))?;
    }
    for target in &spec.targets {
        validate_target_paths(path, target)?;
    }
    for entry in &spec.stage_entries {
        ensure_relative_confined_path(
            &entry.source,
            &format!(
                "DepoFile {} {} source path",
                path.display(),
                match entry.kind {
                    StageKind::File => "STAGE_FILE",
                    StageKind::Tree => "STAGE_TREE",
                }
            ),
        )?;
        ensure_relative_confined_path(
            &entry.destination,
            &format!(
                "DepoFile {} {} destination path",
                path.display(),
                match entry.kind {
                    StageKind::File => "STAGE_FILE",
                    StageKind::Tree => "STAGE_TREE",
                }
            ),
        )?;
    }
    Ok(())
}

fn parse_build_system(path: &Path, line_number: usize, remainder: &str) -> Result<BuildSystem> {
    let value = expect_single_token(path, line_number, "BUILD_SYSTEM", remainder)?;
    match value.as_str() {
        "CMAKE" => Ok(BuildSystem::Cmake),
        "MESON" => Ok(BuildSystem::Meson),
        "AUTOCONF" => Ok(BuildSystem::Autoconf),
        "CARGO" => Ok(BuildSystem::Cargo),
        "MANUAL" => Ok(BuildSystem::Manual),
        _ => bail!(
            "{}:{}: BUILD_SYSTEM requires one of CMAKE, MESON, AUTOCONF, CARGO, or MANUAL",
            path.display(),
            line_number
        ),
    }
}

fn require_build_system(
    path: &Path,
    line_number: usize,
    keyword: &str,
    selected_build_system: Option<BuildSystem>,
    required_build_system: BuildSystem,
) -> Result<()> {
    match selected_build_system {
        Some(selected) if selected == required_build_system => Ok(()),
        Some(selected) => bail!(
            "{}:{}: {} requires BUILD_SYSTEM {}, but DepoFile selects BUILD_SYSTEM {}",
            path.display(),
            line_number,
            keyword,
            required_build_system.directive_name(),
            selected.directive_name()
        ),
        None => bail!(
            "{}:{}: {} requires BUILD_SYSTEM {} and BUILD_SYSTEM must appear before it",
            path.display(),
            line_number,
            keyword,
            required_build_system.directive_name()
        ),
    }
}

fn parse_dependency_request(
    path: &Path,
    line_number: usize,
    remainder: &str,
    default_namespace: &str,
) -> Result<PackageRequest> {
    let tokens = tokenize_arguments(remainder)?;
    if tokens.is_empty() {
        bail!(
            "{}:{}: DEPENDS requires at least a package name",
            path.display(),
            line_number
        );
    }
    let name = tokens[0].clone();
    ensure_package_name(&name)?;

    let mut namespace = default_namespace.to_string();
    let mut inherit_namespace = true;
    let mut mode = RequestMode::Latest;
    let mut source = RequestSource::Auto;
    let mut index = 1usize;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "NAMESPACE" => {
                index += 1;
                let value = tokens.get(index).with_context(|| {
                    format!(
                        "{}:{}: NAMESPACE requires a value",
                        path.display(),
                        line_number
                    )
                })?;
                ensure_namespace_name(value)?;
                namespace = value.clone();
                inherit_namespace = false;
            }
            "VERSION" => {
                index += 1;
                let value = tokens.get(index).with_context(|| {
                    format!(
                        "{}:{}: VERSION requires a value",
                        path.display(),
                        line_number
                    )
                })?;
                ensure_version_name(value)?;
                mode = RequestMode::Exact(value.clone());
            }
            "MIN_VERSION" => {
                index += 1;
                let value = tokens.get(index).with_context(|| {
                    format!(
                        "{}:{}: MIN_VERSION requires a value",
                        path.display(),
                        line_number
                    )
                })?;
                ensure_version_name(value)?;
                mode = RequestMode::Minimum(value.clone());
            }
            "SOURCE" => {
                index += 1;
                let value = tokens.get(index).with_context(|| {
                    format!(
                        "{}:{}: SOURCE requires a value",
                        path.display(),
                        line_number
                    )
                })?;
                source = RequestSource::parse(value)?;
            }
            other => bail!(
                "{}:{}: unsupported DEPENDS token '{}'",
                path.display(),
                line_number,
                other
            ),
        }
        index += 1;
    }
    Ok(PackageRequest {
        name,
        namespace,
        inherit_namespace,
        mode,
        source,
        alias: None,
    })
}

fn parse_target_line(
    path: &Path,
    line_number: usize,
    remainder: &str,
    targets: &mut Vec<TargetSpec>,
) -> Result<()> {
    let parts = tokenize_arguments(remainder)?;
    if parts.len() < 2 {
        bail!(
            "{}:{}: TARGET requires '<name> INTERFACE [include-dir...]' or '<name> [STATIC <path>] [SHARED <path>] [OBJECT <path>] [INTERFACE [include-dir...]]'",
            path.display(),
            line_number
        );
    }
    let name = parts[0].clone();
    let target = target_entry_mut(targets, &name);
    let mut index = 1usize;
    while index < parts.len() {
        match parts[index].as_str() {
            "STATIC" => {
                index += 1;
                let value = parts.get(index).with_context(|| {
                    format!(
                        "{}:{}: TARGET {} STATIC requires an artifact path",
                        path.display(),
                        line_number,
                        name
                    )
                })?;
                if target.static_path.is_some() {
                    bail!(
                        "{}:{}: TARGET {} repeats STATIC",
                        path.display(),
                        line_number,
                        name
                    );
                }
                target.static_path = Some(PathBuf::from(value));
            }
            "SHARED" => {
                index += 1;
                let value = parts.get(index).with_context(|| {
                    format!(
                        "{}:{}: TARGET {} SHARED requires an artifact path",
                        path.display(),
                        line_number,
                        name
                    )
                })?;
                if target.shared_path.is_some() {
                    bail!(
                        "{}:{}: TARGET {} repeats SHARED",
                        path.display(),
                        line_number,
                        name
                    );
                }
                target.shared_path = Some(PathBuf::from(value));
            }
            "OBJECT" => {
                index += 1;
                let value = parts.get(index).with_context(|| {
                    format!(
                        "{}:{}: TARGET {} OBJECT requires an artifact path",
                        path.display(),
                        line_number,
                        name
                    )
                })?;
                if target.object_path.is_some() {
                    bail!(
                        "{}:{}: TARGET {} repeats OBJECT",
                        path.display(),
                        line_number,
                        name
                    );
                }
                target.object_path = Some(PathBuf::from(value));
            }
            "INTERFACE" => {
                target.interface_declared = true;
                if parts[index + 1..]
                    .iter()
                    .any(|value| matches!(value.as_str(), "STATIC" | "SHARED" | "OBJECT" | "INTERFACE"))
                {
                    bail!(
                        "{}:{}: TARGET {} must place INTERFACE last on the line",
                        path.display(),
                        line_number,
                        name
                    );
                }
                target
                    .include_dirs
                    .extend(parts[index + 1..].iter().map(PathBuf::from));
                return Ok(());
            }
            _ => bail!(
                "{}:{}: TARGET requires '<name> INTERFACE [include-dir...]' or '<name> [STATIC <path>] [SHARED <path>] [OBJECT <path>] [INTERFACE [include-dir...]]'",
                path.display(),
                line_number
            ),
        }
        index += 1;
    }
    Ok(())
}

fn parse_target_values_directive(
    path: &Path,
    line_number: usize,
    keyword: &str,
    remainder: &str,
    pending: &mut BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let parts = tokenize_arguments(remainder)?;
    if parts.len() < 2 {
        bail!(
            "{}:{}: {} requires '<target> <value>...'",
            path.display(),
            line_number,
            keyword
        );
    }
    pending
        .entry(parts[0].clone())
        .or_default()
        .extend(parts[1..].iter().cloned());
    Ok(())
}

fn target_entry_mut<'a>(targets: &'a mut Vec<TargetSpec>, name: &str) -> &'a mut TargetSpec {
    if let Some(index) = targets.iter().position(|target| target.name == name) {
        &mut targets[index]
    } else {
        targets.push(TargetSpec::new(name.to_string()));
        targets.last_mut().expect("just pushed target")
    }
}

fn parse_phase_command(
    path: &Path,
    line_number: usize,
    keyword: &str,
    remainder: &str,
) -> Result<Vec<String>> {
    let argv = tokenize_arguments(remainder)?;
    if argv.is_empty() {
        bail!(
            "{}:{}: {} requires at least one command token",
            path.display(),
            line_number,
            keyword
        );
    }
    Ok(argv)
}

fn ensure_phase_override_conflict(
    path: &Path,
    direct_name: &str,
    direct: &Option<Vec<String>>,
    shell_name: &str,
    shell: &Option<String>,
) -> Result<()> {
    if direct.is_some() && shell.is_some() {
        bail!(
            "DepoFile {} declares both {} and {} for the same phase",
            path.display(),
            direct_name,
            shell_name
        );
    }
    Ok(())
}

fn ensure_phase_structured_conflict(
    path: &Path,
    direct_name: &str,
    direct: &Option<Vec<String>>,
    shell: &Option<String>,
    structured_name: &str,
    structured_present: bool,
) -> Result<()> {
    if structured_present && (direct.is_some() || shell.is_some()) {
        bail!(
            "DepoFile {} declares both {} and {} for the same phase",
            path.display(),
            direct_name,
            structured_name
        );
    }
    Ok(())
}

struct BuildSystemCommandInputs<'a> {
    configure_args: &'a [String],
    build_args: &'a [String],
    install_args: &'a [String],
    configure_direct: Option<Vec<String>>,
    build_direct: Option<Vec<String>>,
    install_direct: Option<Vec<String>>,
    configure_override: Option<String>,
    build_override: Option<String>,
    install_override: Option<String>,
}

struct BuildSystemCommands {
    configure: Vec<Vec<String>>,
    build: Vec<Vec<String>>,
    install: Vec<Vec<String>>,
}

fn synthesize_build_system_commands(
    build_system: BuildSystem,
    inputs: BuildSystemCommandInputs<'_>,
) -> BuildSystemCommands {
    let configure = match (inputs.configure_direct, inputs.configure_override) {
        (Some(argv), None) => vec![argv],
        (None, Some(script)) => shell_phase(script),
        (None, None) => match build_system {
            BuildSystem::Cmake => {
                let mut argv = vec![
                    "cmake".to_string(),
                    "-S".to_string(),
                    "${DEPO_SOURCE_DIR}".to_string(),
                    "-B".to_string(),
                    "${DEPO_BUILD_DIR}".to_string(),
                    "-G".to_string(),
                    "Ninja".to_string(),
                    "-DCMAKE_BUILD_TYPE=Release".to_string(),
                    "-DCMAKE_INSTALL_PREFIX=${DEPO_PREFIX}".to_string(),
                    "-DCMAKE_INSTALL_LIBDIR=lib".to_string(),
                ];
                argv.extend(inputs.configure_args.iter().cloned());
                vec![argv]
            }
            BuildSystem::Meson => {
                let mut argv = vec![
                    "meson".to_string(),
                    "setup".to_string(),
                    "${DEPO_BUILD_DIR}".to_string(),
                    "${DEPO_SOURCE_DIR}".to_string(),
                    "--wipe".to_string(),
                    "--buildtype=release".to_string(),
                    "--prefix=${DEPO_PREFIX}".to_string(),
                    "--libdir=lib".to_string(),
                ];
                argv.extend(inputs.configure_args.iter().cloned());
                vec![argv]
            }
            BuildSystem::Autoconf => shell_phase(format!(
                "./configure --prefix=\"$DEPO_PREFIX\" --libdir=\"$DEPO_PREFIX/lib\"{}",
                if inputs.configure_args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", shell_join(inputs.configure_args))
                }
            )),
            BuildSystem::Cargo | BuildSystem::Manual => Vec::new(),
        },
        (Some(_), Some(_)) => unreachable!(),
    };

    let build = match (inputs.build_direct, inputs.build_override) {
        (Some(argv), None) => vec![argv],
        (None, Some(script)) => shell_phase(script),
        (None, None) => match build_system {
            BuildSystem::Cmake => {
                let mut argv = vec![
                    "cmake".to_string(),
                    "--build".to_string(),
                    "${DEPO_BUILD_DIR}".to_string(),
                    "--parallel".to_string(),
                ];
                argv.extend(inputs.build_args.iter().cloned());
                vec![argv]
            }
            BuildSystem::Meson => {
                let mut argv = vec![
                    "meson".to_string(),
                    "compile".to_string(),
                    "-C".to_string(),
                    "${DEPO_BUILD_DIR}".to_string(),
                ];
                argv.extend(inputs.build_args.iter().cloned());
                vec![argv]
            }
            BuildSystem::Autoconf => shell_phase(format!(
                "make -j$(nproc){}",
                if inputs.build_args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", shell_join(inputs.build_args))
                }
            )),
            BuildSystem::Cargo => {
                let mut argv = vec![
                    "cargo".to_string(),
                    "build".to_string(),
                    "--release".to_string(),
                    "--target-dir".to_string(),
                    "${DEPO_BUILD_DIR}/cargo-target".to_string(),
                ];
                argv.extend(inputs.build_args.iter().cloned());
                shell_phase(shell_join(&argv))
            }
            BuildSystem::Manual => Vec::new(),
        },
        (Some(_), Some(_)) => unreachable!(),
    };

    let install = match (inputs.install_direct, inputs.install_override) {
        (Some(argv), None) => vec![argv],
        (None, Some(script)) => shell_phase(script),
        (None, None) => match build_system {
            BuildSystem::Cmake => {
                let mut argv = vec![
                    "cmake".to_string(),
                    "--install".to_string(),
                    "${DEPO_BUILD_DIR}".to_string(),
                ];
                argv.extend(inputs.install_args.iter().cloned());
                vec![argv]
            }
            BuildSystem::Meson => {
                let mut argv = vec![
                    "meson".to_string(),
                    "install".to_string(),
                    "-C".to_string(),
                    "${DEPO_BUILD_DIR}".to_string(),
                ];
                argv.extend(inputs.install_args.iter().cloned());
                vec![argv]
            }
            BuildSystem::Autoconf => shell_phase(format!(
                "make install{}",
                if inputs.install_args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", shell_join(inputs.install_args))
                }
            )),
            BuildSystem::Cargo => {
                if inputs.install_args.is_empty() {
                    Vec::new()
                } else {
                    let mut argv = vec![
                        "cargo".to_string(),
                        "install".to_string(),
                        "--path".to_string(),
                        "${DEPO_SOURCE_DIR}".to_string(),
                        "--root".to_string(),
                        "${DEPO_PREFIX}".to_string(),
                    ];
                    argv.extend(inputs.install_args.iter().cloned());
                    shell_phase(shell_join(&argv))
                }
            }
            BuildSystem::Manual => Vec::new(),
        },
        (Some(_), Some(_)) => unreachable!(),
    };

    BuildSystemCommands {
        configure,
        build,
        install,
    }
}

fn default_cmake_cross_configure_args(target_arch: &str) -> Vec<String> {
    let target_prefix = linux_gnu_toolchain_prefix(target_arch);
    vec![
        "-DCMAKE_SYSTEM_NAME=Linux".to_string(),
        format!("-DCMAKE_SYSTEM_PROCESSOR={target_arch}"),
        format!("-DCMAKE_C_COMPILER={target_prefix}-gcc"),
        format!("-DCMAKE_CXX_COMPILER={target_prefix}-g++"),
        format!("-DCMAKE_AR={target_prefix}-ar"),
        format!("-DCMAKE_RANLIB={target_prefix}-ranlib"),
        format!("-DCMAKE_STRIP={target_prefix}-strip"),
    ]
}

fn ensure_package_name(name: &str) -> Result<()> {
    if !valid_identifier(name) || ensure_safe_path_component(name, "package name").is_err() {
        bail!("invalid package name '{}'", name);
    }
    Ok(())
}

fn ensure_namespace_name(namespace: &str) -> Result<()> {
    if namespace.is_empty() {
        bail!("invalid empty namespace");
    }
    if !namespace
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid namespace '{}'", namespace);
    }
    ensure_safe_path_component(namespace, "namespace")?;
    Ok(())
}

fn ensure_version_name(version: &str) -> Result<()> {
    if !version
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
    {
        bail!("invalid version '{}'", version);
    }
    ensure_safe_path_component(version, "version")?;
    Ok(())
}

fn ensure_safe_path_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.contains(['/', '\\', ':'])
        || value.ends_with([' ', '.'])
        || windows_reserved_path_component(value)
    {
        bail!("invalid {label} '{}'", value);
    }
    let mut components = Path::new(value).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        bail!("invalid {label} '{}'", value);
    }
    Ok(())
}

fn ensure_export_manifest_path(path: &Path, manifest: &Path) -> Result<()> {
    let value = path.to_str().with_context(|| {
        format!(
            "PATH in export manifest {} must be valid UTF-8",
            manifest.display()
        )
    })?;
    if value.is_empty() || path.is_absolute() {
        bail!(
            "PATH '{}' in export manifest {} must be nonempty and relative",
            path.display(),
            manifest.display()
        );
    }
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!(
                "PATH '{}' in export manifest {} must contain only normal relative components",
                path.display(),
                manifest.display()
            );
        };
        ensure_safe_path_component(
            component.to_str().with_context(|| {
                format!(
                    "PATH '{}' in export manifest {} must be valid UTF-8",
                    path.display(),
                    manifest.display()
                )
            })?,
            "export path component",
        )?;
    }
    Ok(())
}

fn dedup_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }
    deduped
}

fn ensure_alias_name(alias: &str) -> Result<()> {
    if !valid_identifier(alias) {
        bail!("invalid alias '{}'", alias);
    }
    Ok(())
}

fn sanitize_env_fragment(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() {
            output.push((byte as char).to_ascii_uppercase());
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "_".to_string()
    } else {
        output
    }
}

fn tokenize_arguments(source: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = source.chars().peekable();
    let mut in_quotes = false;

    while let Some(character) = chars.next() {
        match character {
            '"' => {
                in_quotes = !in_quotes;
            }
            '\\' if in_quotes => {
                let escaped = chars.next().context("dangling escape in quoted string")?;
                current.push(escaped);
            }
            ch if ch.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }

    if in_quotes {
        bail!("unterminated quoted string");
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    Ok(tokens)
}

fn group_catalog_by_key(catalog: &[PackageSpec]) -> BTreeMap<PackageKey, Vec<PackageSpec>> {
    let mut grouped = BTreeMap::<PackageKey, Vec<PackageSpec>>::new();
    for package in catalog {
        grouped
            .entry(package.identity_key())
            .or_default()
            .push(package.clone());
    }
    grouped
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let left_tokens = version_tokens(left);
    let right_tokens = version_tokens(right);
    for index in 0..left_tokens.len().max(right_tokens.len()) {
        let left_token = left_tokens.get(index);
        let right_token = right_tokens.get(index);
        match (left_token, right_token) {
            (Some(VersionToken::Number(left_value)), Some(VersionToken::Number(right_value))) => {
                match left_value.cmp(right_value) {
                    Ordering::Equal => continue,
                    order => return order,
                }
            }
            (Some(VersionToken::Text(left_value)), Some(VersionToken::Text(right_value))) => {
                match left_value.cmp(right_value) {
                    Ordering::Equal => continue,
                    order => return order,
                }
            }
            (Some(VersionToken::Number(_)), Some(VersionToken::Text(_))) => {
                return Ordering::Greater
            }
            (Some(VersionToken::Text(_)), Some(VersionToken::Number(_))) => return Ordering::Less,
            (Some(VersionToken::Number(left_value)), None) => {
                return if *left_value == 0 {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            }
            (Some(VersionToken::Text(left_value)), None) => {
                return if left_value.is_empty() {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            }
            (None, Some(VersionToken::Number(right_value))) => {
                return if *right_value == 0 {
                    Ordering::Equal
                } else {
                    Ordering::Less
                }
            }
            (None, Some(VersionToken::Text(right_value))) => {
                return if right_value.is_empty() {
                    Ordering::Equal
                } else {
                    Ordering::Less
                }
            }
            (None, None) => break,
        }
    }
    left.cmp(right)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum VersionToken {
    Number(u64),
    Text(String),
}

fn version_tokens(value: &str) -> Vec<VersionToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_is_digit: Option<bool> = None;

    for character in value.chars() {
        if matches!(character, '.' | '-' | '_' | '+') {
            if !current.is_empty() {
                push_version_token(&mut tokens, &mut current, current_is_digit);
                current_is_digit = None;
            }
            continue;
        }

        let is_digit = character.is_ascii_digit();
        if current_is_digit == Some(is_digit) || current_is_digit.is_none() {
            current.push(character);
            current_is_digit = Some(is_digit);
        } else {
            push_version_token(&mut tokens, &mut current, current_is_digit);
            current.push(character);
            current_is_digit = Some(is_digit);
        }
    }
    if !current.is_empty() {
        push_version_token(&mut tokens, &mut current, current_is_digit);
    }
    tokens
}

fn push_version_token(
    tokens: &mut Vec<VersionToken>,
    current: &mut String,
    current_is_digit: Option<bool>,
) {
    let value = std::mem::take(current);
    match current_is_digit {
        Some(true) => tokens.push(VersionToken::Number(value.parse().unwrap_or(0))),
        _ => tokens.push(VersionToken::Text(value)),
    }
}

fn read_dir_sorted(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to list directory {}", path.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn registered_depofile_path(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
) -> PathBuf {
    depos_root
        .join("depofiles")
        .join("local")
        .join(name)
        .join(namespace)
        .join(version)
        .join("main.DepoFile")
}

fn embedded_registered_depofile_path(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
) -> PathBuf {
    embedded_depofiles_root(depos_root)
        .join(name)
        .join(namespace)
        .join(version)
        .join("main.DepoFile")
}

fn resolve_registered_depofile_path(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
) -> Result<PathBuf> {
    let local = registered_depofile_path(depos_root, name, namespace, version);
    if local.exists() {
        return Ok(local);
    }
    let embedded = embedded_registered_depofile_path(depos_root, name, namespace, version);
    if embedded.exists() {
        return Ok(embedded);
    }
    bail!(
        "registered DepoFile is missing for '{}[{}]@{}': checked {} and {}",
        name,
        namespace,
        version,
        local.display(),
        embedded.display()
    );
}

fn status_file_path(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
    variant: &str,
) -> PathBuf {
    let root = depos_root
        .join(".run")
        .join("status")
        .join(name)
        .join(namespace);
    if variant == default_variant() {
        root.join(format!("{version}.status"))
    } else {
        root.join(version).join(format!("{variant}.status"))
    }
}

fn log_file_path(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
    variant: &str,
) -> PathBuf {
    let root = depos_root
        .join(".run")
        .join("logs")
        .join(name)
        .join(namespace);
    if variant == default_variant() {
        root.join(format!("{version}.log"))
    } else {
        root.join(version).join(format!("{variant}.log"))
    }
}

fn export_manifest_path(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
    variant: &str,
) -> PathBuf {
    let root = depos_root
        .join(".run")
        .join("exports")
        .join(name)
        .join(namespace);
    if variant == default_variant() {
        root.join(format!("{version}.exports"))
    } else {
        root.join(version).join(format!("{variant}.exports"))
    }
}

fn provenance_file_path(depos_root: &Path, name: &str, namespace: &str, version: &str) -> PathBuf {
    depos_root
        .join(".run")
        .join("provenance")
        .join(name)
        .join(namespace)
        .join(format!("{version}.source"))
}

fn materialization_state_path(
    depos_root: &Path,
    name: &str,
    namespace: &str,
    version: &str,
    variant: &str,
) -> PathBuf {
    let root = depos_root
        .join(".run")
        .join("materialization")
        .join(name)
        .join(namespace);
    if variant == default_variant() {
        root.join(format!("{version}.state"))
    } else {
        root.join(version).join(format!("{variant}.state"))
    }
}

fn prune_empty_ancestors(path: &Path, stop_at: &Path) -> Result<()> {
    if path != stop_at && !path.starts_with(stop_at) {
        bail!(
            "refusing to prune '{}' outside '{}'",
            path.display(),
            stop_at.display()
        );
    }
    let stop_at = stop_at.to_path_buf();
    let mut current = path.to_path_buf();
    while current != stop_at {
        if !current.exists() {
            if let Some(parent) = current.parent() {
                current = parent.to_path_buf();
                continue;
            }
            break;
        }
        let is_empty = fs::read_dir(&current)
            .with_context(|| format!("failed to read directory {}", current.display()))?
            .next()
            .is_none();
        if !is_empty {
            break;
        }
        fs::remove_dir(&current)
            .with_context(|| format!("failed to remove {}", current.display()))?;
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod metadata_safety_tests {
    use super::*;

    #[test]
    fn rejects_coordinates_that_are_not_safe_path_components() {
        for namespace in ["", ".", "..", "a/b", r"a\b", "C:"] {
            assert!(ensure_namespace_name(namespace).is_err(), "{namespace}");
        }
        for version in ["", ".", "..", "a/b", r"a\b", "C:", "1.0.0/../../victim"] {
            assert!(ensure_version_name(version).is_err(), "{version}");
        }
    }

    #[test]
    fn rejects_unconfined_export_manifest_fields() {
        let temp = tempfile::tempdir().expect("create manifest fixture root");
        let manifest = temp.path().join("fixture.exports");
        for contents in [
            "STORE_ROOT relative\nPATH include/demo.h\n",
            "STORE_ROOT /tmp/store\nPATH /tmp/victim\n",
            "STORE_ROOT /tmp/store\nPATH ../victim\n",
            "STORE_ROOT /tmp/store\nPATH .\n",
        ] {
            fs::write(&manifest, contents).expect("write export manifest fixture");
            assert!(
                read_export_manifest_path(&manifest).is_err(),
                "{contents:?}"
            );
        }
    }

    #[test]
    fn forged_manifest_root_cannot_delete_external_files() {
        let temp = tempfile::tempdir().expect("create unregister fixture root");
        let depos_root = temp.path().join("depos");
        let expected =
            variant_package_store_root(&depos_root, &default_variant(), "demo", "release", "1.0.0");
        let external = temp.path().join("external");
        fs::create_dir_all(&expected).expect("create expected store root");
        fs::create_dir_all(&external).expect("create external root");
        fs::write(external.join("victim"), b"keep").expect("write external sentinel");
        let manifest =
            export_manifest_path(&depos_root, "demo", "release", "1.0.0", &default_variant());
        fs::create_dir_all(manifest.parent().unwrap()).expect("create manifest parent");
        fs::write(
            &manifest,
            format!("STORE_ROOT {}\nPATH victim\n", external.display()),
        )
        .expect("write forged manifest");

        assert!(unregister_depofile(&UnregisterOptions {
            depos_root,
            name: "demo".to_string(),
            namespace: "release".to_string(),
            version: "1.0.0".to_string(),
        })
        .is_err());
        assert_eq!(
            fs::read(external.join("victim")).expect("read external sentinel"),
            b"keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn export_cleanup_rejects_intermediate_symlink_escape_and_accepts_root_alias() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("create symlink fixture root");
        let store = temp.path().join("store");
        let external = temp.path().join("external");
        fs::create_dir_all(&store).expect("create store");
        fs::create_dir_all(&external).expect("create external root");
        fs::write(external.join("victim"), b"keep").expect("write external sentinel");
        symlink(&external, store.join("escape")).expect("create escaping store symlink");

        assert!(remove_exported_paths(
            &store,
            &[PathBuf::from("escape/victim")],
            &mut String::new(),
        )
        .is_err());
        assert_eq!(
            fs::read(external.join("victim")).expect("read external sentinel"),
            b"keep"
        );

        let alias = temp.path().join("store-alias");
        symlink(&store, &alias).expect("create store alias");
        assert_eq!(
            destructive_store_root(&alias, &store).expect("accept canonical store alias"),
            fs::canonicalize(&store).expect("canonical store")
        );
    }

    #[cfg(unix)]
    #[test]
    fn metadata_cleanup_rejects_intermediate_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("create metadata cleanup fixture root");
        let depos_root = temp.path().join("depos");
        fs::create_dir(&depos_root).expect("create Depos root");
        for (index, relative_root) in [
            "depofiles/local",
            ".run/exports",
            ".run/status",
            ".run/logs",
            ".run/provenance",
            ".run/materialization",
        ]
        .into_iter()
        .enumerate()
        {
            let owner_root = depos_root.join(relative_root);
            let external = temp.path().join(format!("external-{index}"));
            fs::create_dir_all(&owner_root).expect("create metadata owner root");
            fs::create_dir_all(external.join("release")).expect("create external metadata root");
            let sentinel = external.join("release/victim");
            fs::write(&sentinel, b"keep").expect("write external metadata sentinel");
            symlink(&external, owner_root.join("demo")).expect("create metadata escape symlink");

            assert!(remove_confined_path(
                &depos_root,
                &owner_root,
                &owner_root.join("demo/release/victim"),
            )
            .is_err());
            assert_eq!(
                fs::read(&sentinel).expect("read external metadata sentinel"),
                b"keep"
            );
        }
    }

    #[test]
    fn prune_never_walks_outside_its_stop_root() {
        let temp = tempfile::tempdir().expect("create prune fixture root");
        let stop = temp.path().join("inside");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&stop).expect("create stop root");
        fs::create_dir_all(&outside).expect("create outside root");
        assert!(prune_empty_ancestors(&outside, &stop).is_err());
        assert!(outside.exists());
    }
}

#[cfg(test)]
mod target_platform_tests {
    use super::*;

    #[test]
    fn accepts_only_declared_target_platforms() {
        assert_eq!(TargetPlatform::parse("host").unwrap(), TargetPlatform::Host);
        assert_eq!(TargetPlatform::parse("ios").unwrap(), TargetPlatform::Ios);
        assert_eq!(
            TargetPlatform::parse("ios-simulator").unwrap(),
            TargetPlatform::IosSimulator
        );
        assert!(TargetPlatform::parse("macos").is_err());
        assert!(TargetPlatform::parse("IOS").is_err());
    }

    #[test]
    fn non_host_platforms_require_explicit_recipe_support() {
        let mut spec = builtin_catalog().remove(0);
        assert!(ensure_target_platform_supported(&spec, TargetPlatform::Ios).is_err());
        spec.supported_target_platforms.insert(TargetPlatform::Ios);
        ensure_target_platform_supported(&spec, TargetPlatform::Ios)
            .expect("accept explicitly supported iOS recipe");
    }

    #[test]
    fn parses_explicit_recipe_platform_support() {
        let temp = tempfile::tempdir().expect("create platform DepoFile fixture root");
        let depofile = temp.path().join("fixture.DepoFile");
        fs::write(
            &depofile,
            "NAME demo\nVERSION 1\nTARGET_PLATFORMS host ios ios-simulator\nTARGET demo::demo INTERFACE include\n",
        )
        .expect("write platform DepoFile fixture");
        let spec = parse_depofile(&depofile).expect("parse platform DepoFile fixture");
        assert_eq!(
            spec.supported_target_platforms,
            BTreeSet::from([
                TargetPlatform::Host,
                TargetPlatform::Ios,
                TargetPlatform::IosSimulator,
            ])
        );
    }

    #[test]
    fn apple_platforms_reject_linux_provider_pipelines() {
        let mut spec = builtin_catalog().remove(0);
        spec.supported_target_platforms.insert(TargetPlatform::Ios);
        spec.target_platform = TargetPlatform::Ios;
        spec.build_root =
            BuildRoot::Oci("docker://example.invalid/toolchain@sha256:00".to_string());
        spec.toolchain = ToolchainSource::Rootfs;
        let error = validate_supported_command_pipeline(&spec)
            .expect_err("iOS artifact must not be delegated to a Linux provider");
        assert!(
            format!("{error:#}").contains("native macOS BUILD_ROOT SYSTEM"),
            "{error:#}"
        );
    }

    #[test]
    fn target_platform_separates_variants() {
        assert_eq!(
            variant_for_target("aarch64", TargetPlatform::Host).unwrap(),
            "aarch64-aarch64_v1"
        );
        assert_eq!(
            variant_for_target("aarch64", TargetPlatform::Ios).unwrap(),
            "aarch64-aarch64-ios_v1"
        );
        assert_eq!(
            variant_for_target("aarch64", TargetPlatform::IosSimulator).unwrap(),
            "aarch64-aarch64-ios-simulator_v1"
        );

        assert_eq!(
            variant_for_selected_packages(&[], TargetPlatform::Ios).unwrap(),
            variant_for_target(&host_arch(), TargetPlatform::Ios).unwrap()
        );
    }

    #[test]
    fn exposes_only_the_selected_platform_to_recipes() {
        let temp = tempfile::tempdir().expect("create portable command fixture root");
        let paths = portable_command_paths(temp.path()).expect("create portable command paths");
        let mut spec = builtin_catalog().remove(0);
        spec.target_platform = TargetPlatform::Ios;

        let variables =
            build_portable_command_variables(&spec, &[], &paths).expect("build recipe variables");
        assert_eq!(
            variables.get("DEPO_TARGET_PLATFORM").map(String::as_str),
            Some("ios")
        );

        let environment = build_portable_command_environment(&spec, &[], &paths);
        let selected = environment
            .iter()
            .filter(|(name, _)| name == "DEPO_TARGET_PLATFORM")
            .collect::<Vec<_>>();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1, "ios");
    }
}

#[cfg(all(test, unix))]
mod tar_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use tar::{Builder, EntryType, Header};

    #[test]
    fn extracted_sources_are_owned_by_the_materializing_user() {
        let temp = tempfile::tempdir().expect("create TAR fixture root");
        let archive_path = temp.path().join("libcap.tar");
        let mut archive =
            Builder::new(fs::File::create(&archive_path).expect("create TAR fixture"));
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Directory);
        header.set_mode(0o775);
        header.set_uid(1_000);
        header.set_gid(1_000);
        header.set_size(0);
        header.set_cksum();
        archive
            .append_data(&mut header, "libcap-2.76/libcap/", std::io::empty())
            .expect("append publisher-owned directory");
        archive.finish().expect("finish TAR fixture");

        let source_root = temp.path().join("source");
        prepare_url_source(&archive_path, &source_root, &mut String::new())
            .expect("extract TAR fixture");

        assert_eq!(
            fs::metadata(source_root.join("libcap"))
                .expect("inspect extracted directory")
                .uid(),
            fs::metadata(&source_root)
                .expect("inspect extraction root")
                .uid()
        );
    }
}

#[cfg(test)]
mod url_download_tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration, Instant};

    fn read_request(stream: &mut std::net::TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set request read timeout");
        let mut request = [0_u8; 1_024];
        let _ = stream.read(&mut request);
    }

    #[test]
    fn aborts_stalled_url_download_at_transfer_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind stalled HTTP fixture");
        let address = listener.local_addr().expect("read stalled fixture address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept stalled request");
            read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\n")
                .expect("write stalled response headers");
            stream.flush().expect("flush stalled response headers");
            thread::sleep(Duration::from_millis(750));
        });
        let temp = tempfile::tempdir().expect("create stalled download root");
        let archive = temp.path().join("stalled.tar");
        let started = Instant::now();
        let error = download_url_archive_with_limits(
            &format!("http://{address}/archive"),
            &archive,
            &mut String::new(),
            UrlDownloadLimits {
                bytes: 1_024,
                transfer_timeout: Duration::from_millis(100),
            },
        )
        .expect_err("stalled download should fail");
        let elapsed = started.elapsed();
        server.join().expect("join stalled fixture");

        assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
        assert!(format!("{error:#}").contains("transfer limit"));
    }

    #[test]
    fn aborts_streamed_url_response_at_byte_limit() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind oversized HTTP fixture");
        let address = listener
            .local_addr()
            .expect("read oversized fixture address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept oversized request");
            read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .expect("write oversized response headers");
            let payload = [b'x'; 256];
            for _ in 0..64 {
                if stream.write_all(&payload).is_err() {
                    break;
                }
                let _ = stream.flush();
            }
        });
        let temp = tempfile::tempdir().expect("create oversized download root");
        let archive = temp.path().join("oversized.tar");
        let error = download_url_archive_with_limits(
            &format!("http://{address}/archive"),
            &archive,
            &mut String::new(),
            UrlDownloadLimits {
                bytes: 1_024,
                transfer_timeout: Duration::from_secs(2),
            },
        )
        .expect_err("oversized streamed response should fail");
        server.join().expect("join oversized fixture");

        assert!(format!("{error:#}").contains("transfer limit"));
        assert!(
            fs::metadata(&archive)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
                <= 1_024
        );
    }
}

#[cfg(test)]
mod zip_tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    enum TestEntry<'a> {
        Directory(&'a str),
        File(&'a str, &'a [u8]),
        Symlink(&'a str, &'a str),
    }

    fn write_zip(path: &Path, entries: &[TestEntry<'_>]) {
        write_zip_with_compression(path, entries, CompressionMethod::Deflated);
    }

    fn write_zip_with_compression(
        path: &Path,
        entries: &[TestEntry<'_>],
        compression: CompressionMethod,
    ) {
        let file = fs::File::create(path).expect("create ZIP fixture");
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(compression)
            .unix_permissions(0o755);
        for entry in entries {
            match entry {
                TestEntry::Directory(name) => archive
                    .add_directory(*name, options)
                    .expect("add ZIP directory"),
                TestEntry::File(name, contents) => {
                    archive.start_file(*name, options).expect("add ZIP file");
                    archive.write_all(contents).expect("write ZIP file");
                }
                TestEntry::Symlink(name, target) => archive
                    .add_symlink(*name, *target, options)
                    .expect("add ZIP symlink"),
            }
        }
        archive.finish().expect("finish ZIP fixture");
    }

    fn assert_zip_rejected(entries: &[TestEntry<'_>], expected: &str) {
        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("fixture.zip");
        write_zip(&archive, entries);
        let error = validate_zip_archive(&archive).expect_err("unsafe ZIP should fail");
        let rendered = format!("{error:#}");
        assert!(rendered.contains(expected), "{rendered}");
    }

    #[test]
    fn strips_one_shared_wrapped_zip_root() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("db.zip");
        let source = temp.path().join("source");
        fs::create_dir(&source).expect("create previous source root");
        fs::write(source.join("previous.h"), b"previous").expect("write previous source");
        write_zip(
            &archive,
            &[
                TestEntry::Directory("db-18.1.40/"),
                TestEntry::Directory("db-18.1.40/build_unix/"),
                TestEntry::File("db-18.1.40/build_unix/configure", b"#!/bin/sh\n"),
                TestEntry::File("db-18.1.40/src/db.h", b"/* db */\n"),
            ],
        );

        let mut log = String::new();
        prepare_url_source(&archive, &source, &mut log).expect("extract db.zip fixture");

        assert_eq!(
            fs::read_to_string(source.join("build_unix/configure")).expect("read configure"),
            "#!/bin/sh\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("src/db.h")).expect("read db header"),
            "/* db */\n"
        );
        assert!(!source.join("db-18.1.40").exists());
        assert!(!source.join("previous.h").exists());
    }

    #[test]
    fn preserves_exact_multi_root_db_zip_layout() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("db.zip");
        let source = temp.path().join("source");
        write_zip(
            &archive,
            &[
                TestEntry::File("out/country21.bin", b"current country"),
                TestEntry::File("out/timezone21.bin", b"current timezone"),
                TestEntry::File("out_v1/country21.bin", b"v1 country"),
                TestEntry::File("out_v1/timezone21.bin", b"v1 timezone"),
            ],
        );

        prepare_url_source(&archive, &source, &mut String::new())
            .expect("extract multi-root db.zip fixture");

        assert_eq!(
            fs::read(source.join("out/timezone21.bin")).expect("read current database"),
            b"current timezone"
        );
        assert_eq!(
            fs::read(source.join("out_v1/timezone21.bin")).expect("read v1 database"),
            b"v1 timezone"
        );
    }

    #[test]
    fn rejects_unsafe_zip_paths_and_output_collisions() {
        for (entries, expected) in [
            (vec![TestEntry::File("/absolute", b"")], "must be relative"),
            (
                vec![TestEntry::File("root/../escape", b"")],
                "path traversal",
            ),
            (vec![TestEntry::File("C:/escape", b"")], "Windows drive"),
            (
                vec![TestEntry::File("root/C:/escape", b"")],
                "Windows drive",
            ),
            (vec![TestEntry::File("root/C:escape", b"")], "Windows drive"),
            (vec![TestEntry::File(r"C:\escape", b"")], "backslash"),
            (vec![TestEntry::File(r"\\server\share", b"")], "backslash"),
            (
                vec![TestEntry::File("//server/share", b"")],
                "must be relative",
            ),
            (
                vec![TestEntry::File("root/file:stream", b"")],
                "alternate-stream",
            ),
            (
                vec![TestEntry::File("root/nul\0name", b"")],
                "NUL-containing",
            ),
            (
                vec![TestEntry::File("root/CON.txt", b"")],
                "reserved Windows device",
            ),
            (
                vec![
                    TestEntry::File("root/include/db.h", b"one"),
                    TestEntry::File("root/INCLUDE/db.h", b"two"),
                ],
                "collides",
            ),
            (
                vec![
                    TestEntry::File("root/include", b"file"),
                    TestEntry::File("root/include/db.h", b"child"),
                ],
                "parent path",
            ),
            (
                vec![
                    TestEntry::File("root/include/db.h", b"child"),
                    TestEntry::File("root/include", b"file"),
                ],
                "collides",
            ),
            (
                vec![
                    TestEntry::File("root/caf\u{e9}.h", b"one"),
                    TestEntry::File("root/cafe\u{301}.h", b"two"),
                ],
                "collides",
            ),
        ] {
            assert_zip_rejected(&entries, expected);
        }
    }

    #[test]
    fn rejects_zip_symlinks_and_special_entries() {
        assert_zip_rejected(
            &[TestEntry::Symlink("root/escape", "../../outside")],
            "symbolic link",
        );

        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("special.zip");
        write_zip(&archive, &[TestEntry::File("root/fifo", b"")]);
        let mut bytes = fs::read(&archive).expect("read ZIP fixture");
        let central = bytes
            .windows(4)
            .rposition(|window| window == [0x50, 0x4b, 0x01, 0x02])
            .expect("central directory entry");
        bytes[central + 5] = 3;
        bytes[central + 38..central + 42].copy_from_slice(&(0o010600_u32 << 16).to_le_bytes());
        fs::write(&archive, bytes).expect("patch ZIP special entry mode");
        let error = validate_zip_archive(&archive).expect_err("special ZIP entry should fail");
        assert!(format!("{error:#}").contains("unsupported special file type"));
    }

    #[test]
    fn rejects_zip_bomb_limits_before_extraction() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("bomb.zip");
        let first_payload = vec![b'0'; 4_096];
        let second_payload = vec![b'1'; 4_096];
        write_zip(
            &archive,
            &[
                TestEntry::File("root/one.h", &first_payload),
                TestEntry::File("root/two.h", &second_payload),
            ],
        );

        for (limits, expected) in [
            (
                ZipExtractionLimits {
                    entries: 1,
                    ..DEPENDENCY_ZIP_LIMITS
                },
                "more than 1 entries",
            ),
            (
                ZipExtractionLimits {
                    member_bytes: 4_095,
                    ..DEPENDENCY_ZIP_LIMITS
                },
                "4095 byte member limit",
            ),
            (
                ZipExtractionLimits {
                    total_bytes: 8_191,
                    ..DEPENDENCY_ZIP_LIMITS
                },
                "8191 byte total limit",
            ),
            (
                ZipExtractionLimits {
                    compression_ratio: 1,
                    ..DEPENDENCY_ZIP_LIMITS
                },
                "1:1 compression-ratio limit",
            ),
            (
                ZipExtractionLimits {
                    archive_bytes: fs::metadata(&archive).unwrap().len() - 1,
                    ..DEPENDENCY_ZIP_LIMITS
                },
                "archive limit",
            ),
            (
                ZipExtractionLimits {
                    path_bytes: 4,
                    ..DEPENDENCY_ZIP_LIMITS
                },
                "4 byte path limit",
            ),
        ] {
            let error = validate_zip_archive_with_limits(&archive, limits)
                .expect_err("bounded ZIP should fail validation");
            let rendered = format!("{error:#}");
            assert!(rendered.contains(expected), "{rendered}");
        }
    }

    #[test]
    fn extraction_failure_preserves_previous_source_tree() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("corrupt.zip");
        let source = temp.path().join("source");
        fs::create_dir(&source).expect("create previous source root");
        fs::write(source.join("previous.h"), b"previous\n").expect("write previous source");
        let payload = b"replacement-payload-with-unique-bytes";
        write_zip_with_compression(
            &archive,
            &[TestEntry::File("root/replacement.h", payload)],
            CompressionMethod::Stored,
        );
        let mut bytes = fs::read(&archive).expect("read ZIP fixture");
        let payload_offset = bytes
            .windows(payload.len())
            .position(|window| window == payload)
            .expect("stored ZIP payload");
        bytes[payload_offset] ^= 0xff;
        fs::write(&archive, bytes).expect("corrupt ZIP payload");

        let error = prepare_url_source(&archive, &source, &mut String::new())
            .expect_err("corrupt ZIP extraction should fail");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("checksum") || rendered.contains("CRC"),
            "{rendered}"
        );
        assert_eq!(
            fs::read(source.join("previous.h")).expect("read preserved source"),
            b"previous\n"
        );
        assert!(!source.join("replacement.h").exists());
        let staging_entries = fs::read_dir(temp.path())
            .expect("read temp root")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("depos-staging")
            })
            .count();
        assert_eq!(staging_entries, 0);
    }

    #[test]
    fn truncated_zip_preserves_previous_source_tree() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("truncated.zip");
        let source = temp.path().join("source");
        write_zip(&archive, &[TestEntry::File("root/replacement.h", b"new")]);
        let mut bytes = fs::read(&archive).expect("read ZIP fixture");
        bytes.truncate(bytes.len().saturating_sub(16));
        fs::write(&archive, bytes).expect("truncate ZIP fixture");
        fs::create_dir(&source).expect("create previous source root");
        fs::write(source.join("previous.h"), b"previous").expect("write previous source");

        prepare_url_source(&archive, &source, &mut String::new())
            .expect_err("truncated ZIP extraction should fail");
        assert_eq!(
            fs::read(source.join("previous.h")).expect("read preserved source"),
            b"previous"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checksum_mismatch_preserves_previous_archive_until_validated_replacement() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let downloaded = temp.path().join("downloaded.zip");
        let cached = temp.path().join("cached.zip");
        write_zip(
            &downloaded,
            &[TestEntry::File("root/current.h", b"current")],
        );
        write_zip(&cached, &[TestEntry::File("root/previous.h", b"previous")]);
        let previous = fs::read(&cached).expect("read previous archive");
        let url = format!("file://{}", downloaded.display());

        let error = resolve_url_source(&url, Some(&"0".repeat(64)), &cached, &mut String::new())
            .expect_err("checksum mismatch should fail");
        assert!(format!("{error:#}").contains("sha256 mismatch"));
        assert_eq!(fs::read(&cached).expect("read preserved archive"), previous);

        let expected = hash_file_sha256(&downloaded).expect("hash replacement archive");
        assert_eq!(
            resolve_url_source(&url, Some(&expected), &cached, &mut String::new())
                .expect("publish validated replacement archive"),
            expected
        );
        assert_eq!(
            fs::read(&cached).expect("read replacement archive"),
            fs::read(&downloaded).expect("read source archive")
        );
        let leftovers = fs::read_dir(temp.path())
            .expect("read archive cache directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.contains("depos-download") || name.contains("depos-previous")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_archive_without_replacing_source() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("real.zip");
        let archive_link = temp.path().join("linked.zip");
        let source = temp.path().join("source");
        write_zip(&archive, &[TestEntry::File("root/replacement.h", b"new")]);
        symlink(&archive, &archive_link).expect("create archive symlink");
        fs::create_dir(&source).expect("create source root");
        fs::write(source.join("previous.h"), b"previous").expect("write previous source");

        let error = prepare_url_source(&archive_link, &source, &mut String::new())
            .expect_err("symlink ZIP should fail");
        assert!(format!("{error:#}").contains("non-symlink regular file"));
        assert_eq!(
            fs::read(source.join("previous.h")).expect("read preserved source"),
            b"previous"
        );

        let outside = temp.path().join("outside");
        let source_link = temp.path().join("source-link");
        fs::create_dir(&outside).expect("create symlink target root");
        fs::write(outside.join("previous.h"), b"outside").expect("write symlink target fixture");
        symlink(&outside, &source_link).expect("create source-root symlink");
        let error = prepare_url_source(&archive, &source_link, &mut String::new())
            .expect_err("symlink source root should fail");
        assert!(format!("{error:#}").contains("must be a real directory"));
        assert_eq!(
            fs::read(outside.join("previous.h")).expect("read untouched symlink target"),
            b"outside"
        );
    }

    #[test]
    fn rejects_unsupported_compression_before_creating_extraction_root() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("unsupported.zip");
        let source = temp.path().join("source");
        write_zip(&archive, &[TestEntry::File("root/file", b"payload")]);
        let mut bytes = fs::read(&archive).expect("read ZIP fixture");
        let local = bytes
            .windows(4)
            .position(|window| window == [0x50, 0x4b, 0x03, 0x04])
            .expect("local ZIP header");
        let central = bytes
            .windows(4)
            .position(|window| window == [0x50, 0x4b, 0x01, 0x02])
            .expect("central ZIP header");
        bytes[local + 8..local + 10].copy_from_slice(&12_u16.to_le_bytes());
        bytes[central + 10..central + 12].copy_from_slice(&12_u16.to_le_bytes());
        fs::write(&archive, bytes).expect("patch ZIP compression method");

        let error = prepare_url_source(&archive, &source, &mut String::new())
            .expect_err("unsupported ZIP compression should fail before extraction");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("unsupported ZIP compression method"),
            "{rendered}"
        );
        assert!(!source.exists());
    }

    #[test]
    fn path_traversal_is_rejected_before_extraction() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let archive = temp.path().join("escape.zip");
        let source = temp.path().join("source");
        let outside = temp.path().join("outside");
        write_zip(
            &archive,
            &[TestEntry::File("root/../../outside", b"escaped")],
        );

        prepare_url_source(&archive, &source, &mut String::new())
            .expect_err("traversal ZIP should fail");
        assert!(!outside.exists());
        assert!(!source.exists());
    }
}
