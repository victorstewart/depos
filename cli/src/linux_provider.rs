// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use crate::{
    canonical_path, package_store_root, registered_depofile_path, remove_existing_path,
    resolve_dependency_specs, variant_for_target_arch, PackageOrigin, PackageSpec,
};
use anyhow::{anyhow, bail, Context, Result};
use metalor::runtime::linux_provider::{
    LocalLinuxProviderKind, LocalLinuxProviderSelection, ProviderRuntimeLayout, ProviderSession,
    ProviderShell, PROVIDER_RUNTIME_LAYOUT_VERSION,
};
#[cfg(target_os = "macos")]
use metalor::runtime::macos::{AppleLinuxProvider, DEFAULT_APPLE_LINUX_BUNDLE};
#[cfg(target_os = "windows")]
use metalor::runtime::windows::{resolve_wsl_distro, WslProvider};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug)]
enum ProviderBackend {
    #[cfg(target_os = "windows")]
    Wsl {
        provider: WslProvider,
        auto_install: bool,
    },
    #[cfg(target_os = "macos")]
    AppleVirtualization { provider: AppleLinuxProvider },
}

#[derive(Clone, Debug)]
struct LinuxProvider {
    backend: ProviderBackend,
    runtime: ProviderRuntimeLayout,
    session: ProviderSession<ProviderBackend>,
    cache_root: String,
    repo_parent: String,
    repo_root: String,
    target_root: String,
    binary_path: String,
}

struct ProviderJob {
    root: String,
    depos_root: String,
    variant: String,
    variant_root: String,
    store_root: String,
}

struct ProviderSourceBundle {
    local_root: PathBuf,
    fingerprint: String,
}

const PROVIDER_BOOTSTRAP_VERSION: &str = "v1";

impl ProviderBackend {
    fn kind(&self) -> LocalLinuxProviderKind {
        match self {
            #[cfg(target_os = "windows")]
            Self::Wsl { .. } => LocalLinuxProviderKind::Wsl2,
            #[cfg(target_os = "macos")]
            Self::AppleVirtualization { .. } => LocalLinuxProviderKind::MacLocal,
        }
    }

    fn identity(&self) -> &str {
        match self {
            #[cfg(target_os = "windows")]
            Self::Wsl { provider, .. } => provider.distro(),
            #[cfg(target_os = "macos")]
            Self::AppleVirtualization { provider } => provider.vm_name(),
        }
    }

    fn ensure_available(&self, log: &mut String) -> Result<()> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Wsl {
                provider,
                auto_install,
            } => provider.ensure_available(*auto_install, log).with_context(|| {
                format!(
                    "failed to use WSL distro '{}'; install/configure WSL and set DEPOS_WSL_DISTRO if needed",
                    provider.distro()
                )
            }),
            #[cfg(target_os = "macos")]
            Self::AppleVirtualization { provider } => provider
                .ensure_available(DEFAULT_APPLE_LINUX_BUNDLE, log)
                .with_context(|| {
                    format!(
                        "failed to use Apple Virtualization helper {}; ensure it can create or resume Linux VM '{}'",
                        provider.helper().display(),
                        provider.vm_name()
                    )
                }),
        }
    }
}

impl ProviderShell for ProviderBackend {
    fn spawn_shell(&self, script: &str) -> Result<Command> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Wsl { provider, .. } => provider.spawn_shell(script),
            #[cfg(target_os = "macos")]
            Self::AppleVirtualization { provider } => provider.spawn_shell(script),
        }
    }
}

pub(crate) fn execute_linux_provider_command_pipeline(
    depos_root: &Path,
    store_root: &Path,
    spec: &PackageSpec,
    source_root: &Path,
    log: &mut String,
) -> Result<Vec<PathBuf>> {
    let _provider_guard = provider_process_lock()
        .lock()
        .map_err(|_| anyhow!("linux provider process lock was poisoned"))?;
    let provider = LinuxProvider::detect()?;
    let repo_root = provider_source_repo()?;
    provider.ensure_bootstrap(&repo_root, log)?;

    let dependency_specs = resolve_dependency_specs(depos_root, spec)?;
    let job = provider.create_job(spec, log)?;

    stage_registered_depofile(&provider, depos_root, spec, &job.depos_root, log)?;
    for dependency in &dependency_specs {
        if dependency.origin == PackageOrigin::Local {
            stage_registered_depofile(&provider, depos_root, dependency, &job.depos_root, log)?;
        }
        let dependency_store_root = package_store_root(depos_root, &job.variant, dependency);
        provider.session.stage_host_path(
            &dependency_store_root,
            &remote_store_parent(&job.variant_root, dependency),
            true,
            log,
        )?;
    }

    provider
        .session
        .stage_host_path(source_root, &job.root, false, log)?;
    let source_basename = file_name_string(source_root)?;
    let remote_source_root = format!("{}/{}", job.root, source_basename);

    provider.run_materialize(spec, &job, &remote_source_root, log)?;

    if let Some(parent) = store_root.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        remove_existing_path(store_root)?;
        provider
            .session
            .collect_path(&job.store_root, parent, log)?;
    } else {
        bail!("store root must have a parent: {}", store_root.display());
    }

    provider.session.remove_path(&job.root, log)?;
    Ok(spec.required_paths())
}

fn provider_process_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn stage_registered_depofile(
    provider: &LinuxProvider,
    depos_root: &Path,
    spec: &PackageSpec,
    provider_depos_root: &str,
    log: &mut String,
) -> Result<()> {
    let depofile = registered_depofile_path(depos_root, &spec.name, &spec.namespace, &spec.version);
    provider.session.stage_host_path(
        &depofile,
        &remote_depofile_parent(provider_depos_root, spec),
        false,
        log,
    )?;
    Ok(())
}

fn remote_depofile_parent(provider_depos_root: &str, spec: &PackageSpec) -> String {
    format!(
        "{}/depofiles/local/{}/{}/{}",
        provider_depos_root, spec.name, spec.namespace, spec.version
    )
}

fn remote_store_parent(variant_root: &str, spec: &PackageSpec) -> String {
    format!("{}/{}/{}", variant_root, spec.name, spec.namespace)
}

fn remote_store_root(variant_root: &str, spec: &PackageSpec) -> String {
    format!(
        "{}/{}/{}/{}",
        variant_root, spec.name, spec.namespace, spec.version
    )
}

impl LinuxProvider {
    fn detect() -> Result<Self> {
        let selection = configured_provider_selection()?;
        let source_repo = provider_source_repo()?;
        let runtime = configured_provider_runtime_layout(&source_repo)?;
        let cache_root = runtime.join("toolchain-cache")?;
        let repo_parent = runtime.join("repo-source")?;
        let repo_root = format!("{repo_parent}/{}", file_name_string(&source_repo)?);
        let target_root = runtime.join("cargo-target")?;
        let binary_path = format!("{target_root}/release/depos");
        let backend;
        #[cfg(target_os = "windows")]
        {
            if selection == LocalLinuxProviderSelection::MacLocal {
                bail!(
                    "DEPOS_LINUX_PROVIDER=mac-local is not supported on Windows; use auto or wsl2"
                );
            }
            let resolution = resolve_wsl_distro(std::env::var("DEPOS_WSL_DISTRO").ok().as_deref())?;
            backend = ProviderBackend::Wsl {
                provider: WslProvider::new(resolution.distro)?,
                auto_install: resolution.auto_install,
            };
        }
        #[cfg(target_os = "macos")]
        {
            if selection == LocalLinuxProviderSelection::Wsl2 {
                bail!("DEPOS_LINUX_PROVIDER=wsl2 is not supported on macOS; use auto or mac-local");
            }
            let helper = std::env::var_os("DEPOS_APPLE_VIRTUALIZATION_HELPER")
                .map(PathBuf::from)
                .ok_or_else(|| {
                    anyhow!(
                        "BUILD_ROOT OCI on macOS requires a direct Apple Virtualization helper; set DEPOS_APPLE_VIRTUALIZATION_HELPER to the helper executable"
                    )
                })?;
            if !helper.is_absolute() {
                bail!(
                    "DEPOS_APPLE_VIRTUALIZATION_HELPER must be an absolute path: {}",
                    helper.display()
                );
            }
            let vm_name = std::env::var("DEPOS_APPLE_VIRTUALIZATION_VM")
                .unwrap_or_else(|_| "depos".to_string());
            backend = ProviderBackend::AppleVirtualization {
                provider: AppleLinuxProvider::new(helper, vm_name)?,
            };
        }
        let session = ProviderSession::new(backend.clone());
        Ok(Self {
            backend,
            runtime,
            session,
            cache_root,
            repo_parent,
            repo_root,
            target_root,
            binary_path,
        })
    }

    fn create_job(&self, spec: &PackageSpec, log: &mut String) -> Result<ProviderJob> {
        let variant = variant_for_target_arch(&spec.target_arch)?;
        let root = self
            .session
            .prepare_job_root(&self.runtime, &spec.name, log)?
            .root;
        let depos_root = format!("{root}/depos-root");
        let variant_root = format!("{depos_root}/store/{variant}");
        let store_root = remote_store_root(&variant_root, spec);
        self.session.run(
            &format!(
                "mkdir -p {variant_root}",
                variant_root = shell_quote(&variant_root),
            ),
            log,
        )?;
        Ok(ProviderJob {
            root: root.clone(),
            depos_root,
            variant,
            variant_root,
            store_root,
        })
    }

    fn ensure_bootstrap(&self, repo_root: &Path, log: &mut String) -> Result<()> {
        self.backend.ensure_available(log)?;
        self.session.run(
            &format!(
                "mkdir -p {} {} {}",
                shell_quote(self.runtime.root()),
                shell_quote(&self.cache_root),
                shell_quote(&self.repo_parent),
            ),
            log,
        )?;
        let metadata = self.runtime.metadata(
            self.backend.kind(),
            self.backend.identity(),
            PROVIDER_BOOTSTRAP_VERSION,
        )?;
        self.session.write_runtime_metadata(&metadata, log)?;
        let source_bundle = prepare_provider_source_bundle(repo_root)?;
        let repo_sync_stamp = self
            .runtime
            .stamp_path("repo-sync", &source_bundle.fingerprint)?;
        self.session.ensure_warm_state(
            "source sync",
            &repo_sync_stamp,
            &[&self.repo_root],
            log,
            |session, log| {
                session.remove_path(&self.repo_root, log)?;
                session.stage_host_path(
                    &source_bundle.local_root,
                    &self.repo_parent,
                    false,
                    log,
                )?;
                Ok(())
            },
        )?;
        self.session.run(
            &bootstrap_script(
                self.runtime.root(),
                &self.repo_root,
                &self.target_root,
                &self.binary_path,
                &source_bundle.fingerprint,
            ),
            log,
        )
    }

    fn run_materialize(
        &self,
        spec: &PackageSpec,
        job: &ProviderJob,
        remote_source_root: &str,
        log: &mut String,
    ) -> Result<()> {
        let script = format!(
            "DEPOS_INTERNAL_LINUX_PROVIDER=1 DEPOS_PROVIDER_CACHE_ROOT={cache_root} exec {binary} internal-materialize-prepared --depos-root {depos_root} --name {name} --namespace {namespace} --version {version} --source-root {source_root} --store-root {store_root} --executable {binary}",
            cache_root = shell_quote(&self.cache_root),
            binary = shell_quote(&self.binary_path),
            depos_root = shell_quote(&job.depos_root),
            name = shell_quote(&spec.name),
            namespace = shell_quote(&spec.namespace),
            version = shell_quote(&spec.version),
            source_root = shell_quote(remote_source_root),
            store_root = shell_quote(&job.store_root),
        );
        self.session.run(&script, log)
    }
}

fn bootstrap_script(
    runtime_root: &str,
    repo_root: &str,
    target_root: &str,
    binary_path: &str,
    source_fingerprint: &str,
) -> String {
    format!(
        r#"
set -eu
SUDO=
if [ "$(id -u)" -ne 0 ]; then
  SUDO=sudo
fi
$SUDO mkdir -p /proc/sys/fs/binfmt_misc
if [ ! -e /proc/sys/fs/binfmt_misc/register ]; then
  $SUDO mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc || true
fi
$SUDO mkdir -p {runtime_root}
if [ ! -f {bootstrap_stamp} ]; then
  echo "provider bootstrap: cold"
  $SUDO apt-get update
  $SUDO DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential clang cmake curl git pkg-config tar umoci skopeo qemu-user-static ca-certificates
  $SUDO touch {bootstrap_stamp}
else
  echo "provider bootstrap: warm"
fi
if ! command -v cargo >/dev/null 2>&1; then
  curl --fail --location --silent --show-error https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
export PATH="$HOME/.cargo/bin:$PATH"
if [ ! -f {binary_build_stamp} ] || [ ! -x {binary_path} ]; then
  echo "provider binary build: cold"
  cargo build --release --locked --manifest-path {manifest} --target-dir {target}
  touch {binary_build_stamp}
else
  echo "provider binary build: warm"
fi
"#,
        runtime_root = shell_quote(runtime_root),
        bootstrap_stamp = shell_quote(&format!(
            "{runtime_root}/bootstrap-{PROVIDER_BOOTSTRAP_VERSION}.stamp"
        )),
        manifest = shell_quote(&format!("{repo_root}/cli/Cargo.toml")),
        target = shell_quote(target_root),
        binary_path = shell_quote(binary_path),
        binary_build_stamp = shell_quote(&format!(
            "{runtime_root}/binary-build-{source_fingerprint}.stamp"
        )),
    )
}

fn prepare_provider_source_bundle(source_repo: &Path) -> Result<ProviderSourceBundle> {
    let fingerprint = provider_source_bundle_fingerprint(source_repo)?;
    let bundle_parent = std::env::temp_dir()
        .join("depos-provider-source-bundles")
        .join(&fingerprint);
    let bundle_root = bundle_parent.join(file_name_string(source_repo)?);
    if !bundle_root.join("cli/Cargo.toml").is_file() {
        remove_existing_path(&bundle_parent)?;
        copy_provider_source_bundle(source_repo, &bundle_root)?;
    }
    Ok(ProviderSourceBundle {
        local_root: bundle_root,
        fingerprint,
    })
}

fn provider_source_bundle_fingerprint(source_repo: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_provider_source_entry(&mut hasher, source_repo, Path::new("Cargo.toml"))?;
    hash_provider_source_entry(&mut hasher, source_repo, Path::new("Cargo.lock"))?;
    if source_repo.join(".cargo").exists() {
        hash_provider_source_entry(&mut hasher, source_repo, Path::new(".cargo"))?;
    }
    hash_provider_source_entry(&mut hasher, source_repo, Path::new("cli/Cargo.toml"))?;
    hash_provider_source_entry(&mut hasher, source_repo, Path::new("cli/src"))?;
    Ok(format!("{:x}", hasher.finalize())[..16].to_string())
}

fn hash_provider_source_entry(
    hasher: &mut Sha256,
    source_repo: &Path,
    relative: &Path,
) -> Result<()> {
    let path = source_repo.join(relative);
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.is_dir() {
        hasher.update(b"dir\0");
        hasher.update(relative.display().to_string().as_bytes());
        hasher.update([0]);
        let mut entries = fs::read_dir(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("failed to read {}", path.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let child_relative = relative.join(entry.file_name());
            hash_provider_source_entry(hasher, source_repo, &child_relative)?;
        }
    } else {
        hasher.update(b"file\0");
        hasher.update(relative.display().to_string().as_bytes());
        hasher.update([0]);
        hasher
            .update(fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?);
    }
    Ok(())
}

fn copy_provider_source_bundle(source_repo: &Path, bundle_root: &Path) -> Result<()> {
    fs::create_dir_all(bundle_root)
        .with_context(|| format!("failed to create {}", bundle_root.display()))?;
    copy_provider_source_entry(source_repo, bundle_root, Path::new("Cargo.toml"))?;
    copy_provider_source_entry(source_repo, bundle_root, Path::new("Cargo.lock"))?;
    if source_repo.join(".cargo").exists() {
        copy_provider_source_entry(source_repo, bundle_root, Path::new(".cargo"))?;
    }
    copy_provider_source_entry(source_repo, bundle_root, Path::new("cli/Cargo.toml"))?;
    copy_provider_source_entry(source_repo, bundle_root, Path::new("cli/src"))?;
    Ok(())
}

fn copy_provider_source_entry(
    source_repo: &Path,
    bundle_root: &Path,
    relative: &Path,
) -> Result<()> {
    let source = source_repo.join(relative);
    let destination = bundle_root.join(relative);
    let metadata =
        fs::metadata(&source).with_context(|| format!("failed to stat {}", source.display()))?;
    if metadata.is_dir() {
        fs::create_dir_all(&destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        let mut entries = fs::read_dir(&source)
            .with_context(|| format!("failed to read {}", source.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("failed to read {}", source.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            copy_provider_source_entry(
                source_repo,
                bundle_root,
                &relative.join(entry.file_name()),
            )?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "failed to copy provider source bundle entry {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn provider_source_repo() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DEPOS_SOURCE_REPO") {
        let path = canonical_path(Path::new(&path)).with_context(|| {
            format!(
                "failed to access DEPOS_SOURCE_REPO {}",
                PathBuf::from(path).display()
            )
        })?;
        validate_source_repo(&path)?;
        return Ok(path);
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent().unwrap_or(manifest_dir.as_path());
    if repo_root.join("cli/Cargo.toml").is_file() {
        let repo_root = canonical_path(repo_root)?;
        validate_source_repo(&repo_root)?;
        return Ok(repo_root);
    }
    bail!(
        "Linux provider bootstrap needs a depos source checkout; set DEPOS_SOURCE_REPO to the repo root"
    );
}

fn validate_source_repo(path: &Path) -> Result<()> {
    if !path.join("cli/Cargo.toml").is_file() {
        bail!(
            "expected a depos source checkout at {}, but cli/Cargo.toml was missing",
            path.display()
        );
    }
    Ok(())
}

fn runtime_hash_string(path: &Path) -> String {
    let digest = Sha256::digest(path.display().to_string().as_bytes());
    format!("{:x}", digest)[..16].to_string()
}

fn configured_provider_runtime_layout(source_repo: &Path) -> Result<ProviderRuntimeLayout> {
    let root = if let Some(raw) = std::env::var_os("DEPOS_LINUX_PROVIDER_ROOT") {
        raw.to_string_lossy().trim().to_string()
    } else {
        let runtime_hash = runtime_hash_string(source_repo);
        format!("/var/tmp/depos-provider/{PROVIDER_RUNTIME_LAYOUT_VERSION}/{runtime_hash}")
    };
    ProviderRuntimeLayout::new(root)
}

fn configured_provider_selection() -> Result<LocalLinuxProviderSelection> {
    let Some(raw) = std::env::var_os("DEPOS_LINUX_PROVIDER") else {
        return Ok(LocalLinuxProviderSelection::Auto);
    };
    LocalLinuxProviderSelection::from_str(raw.to_string_lossy().trim())
}

fn file_name_string(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("path must have a normal file name: {}", path.display()))
}

fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}
