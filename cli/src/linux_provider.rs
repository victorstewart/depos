// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use crate::{
    append_process_failure_output, append_process_output, canonical_path, package_store_root,
    registered_depofile_path, remove_existing_path, resolve_dependency_specs,
    variant_for_target_arch, PackageOrigin, PackageSpec,
};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
enum ProviderKind {
    #[cfg(target_os = "windows")]
    Wsl { distro: String },
    #[cfg(target_os = "macos")]
    AppleVirtualization { helper: PathBuf, vm_name: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderSelection {
    Auto,
    Wsl2,
    MacLocal,
}

#[derive(Clone, Debug)]
struct LinuxProvider {
    kind: ProviderKind,
    runtime_root: String,
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

const PROVIDER_RUNTIME_LAYOUT_VERSION: &str = "v1";
const PROVIDER_BOOTSTRAP_VERSION: &str = "v1";

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
        provider.push_path(
            &dependency_store_root,
            &remote_store_parent(&job.variant_root, dependency),
            log,
        )?;
    }

    provider.push_path(source_root, &job.root, log)?;
    let source_basename = file_name_string(source_root)?;
    let remote_source_root = format!("{}/{}", job.root, source_basename);

    provider.run_materialize(spec, &job, &remote_source_root, log)?;

    if let Some(parent) = store_root.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        remove_existing_path(store_root)?;
        provider.pull_path(&job.store_root, parent, log)?;
    } else {
        bail!("store root must have a parent: {}", store_root.display());
    }

    provider.run_shell(&format!("rm -rf {}", shell_quote(&job.root)), log)?;
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
    provider.push_path(
        &depofile,
        &remote_depofile_parent(provider_depos_root, spec),
        log,
    )
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
        let runtime_root = configured_provider_runtime_root(&source_repo)?;
        let cache_root = format!("{runtime_root}/toolchain-cache");
        let repo_parent = format!("{runtime_root}/repo-source");
        let source_root = source_repo;
        let repo_root = format!("{repo_parent}/{}", file_name_string(&source_root)?);
        let target_root = format!("{runtime_root}/cargo-target");
        let binary_path = format!("{target_root}/release/depos");
        #[cfg(target_os = "windows")]
        {
            if selection == ProviderSelection::MacLocal {
                bail!(
                    "DEPOS_LINUX_PROVIDER=mac-local is not supported on Windows; use auto or wsl2"
                );
            }
            let distro = detect_wsl_distro()?;
            return Ok(Self {
                kind: ProviderKind::Wsl { distro },
                runtime_root,
                cache_root,
                repo_parent,
                repo_root,
                target_root,
                binary_path,
            });
        }
        #[cfg(target_os = "macos")]
        {
            if selection == ProviderSelection::Wsl2 {
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
            return Ok(Self {
                kind: ProviderKind::AppleVirtualization { helper, vm_name },
                runtime_root,
                cache_root,
                repo_parent,
                repo_root,
                target_root,
                binary_path,
            });
        }
    }

    fn create_job(&self, spec: &PackageSpec, log: &mut String) -> Result<ProviderJob> {
        let variant = variant_for_target_arch(&spec.target_arch)?;
        let job_suffix = format!(
            "{}-{}-{}",
            spec.name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let root = format!("{}/jobs/{job_suffix}", self.runtime_root);
        let depos_root = format!("{root}/depos-root");
        let variant_root = format!("{depos_root}/store/{variant}");
        let store_root = remote_store_root(&variant_root, spec);
        self.run_shell(
            &format!(
                "rm -rf {root} && mkdir -p {variant_root}",
                root = shell_quote(&root),
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
        self.ensure_available(log)?;
        self.run_shell(
            &format!(
                "mkdir -p {} {} {}",
                shell_quote(&self.runtime_root),
                shell_quote(&self.cache_root),
                shell_quote(&self.repo_parent),
            ),
            log,
        )?;
        self.run_shell(&format!("rm -rf {}", shell_quote(&self.repo_root)), log)?;
        self.push_path(repo_root, &self.repo_parent, log)?;
        self.run_shell(
            &bootstrap_script(&self.runtime_root, &self.repo_root, &self.target_root),
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
        self.run_shell(&script, log)
    }

    fn ensure_available(&self, log: &mut String) -> Result<()> {
        match &self.kind {
            #[cfg(target_os = "windows")]
            ProviderKind::Wsl { distro } => {
                self.run_host_command(
                    "wsl.exe",
                    ["-d", distro.as_str(), "--", "bash", "-lc", "true"],
                    log,
                )
                .with_context(|| {
                    format!(
                        "failed to use WSL distro '{}'; install/configure WSL and set DEPOS_WSL_DISTRO if needed",
                        distro
                    )
                })?;
            }
            #[cfg(target_os = "macos")]
            ProviderKind::AppleVirtualization { helper, vm_name } => {
                self.run_host_command_vec_path(
                    helper,
                    vec![
                        "ensure".to_string(),
                        "--vm-name".to_string(),
                        vm_name.clone(),
                        "--bundle".to_string(),
                        "ubuntu-24.04".to_string(),
                    ],
                    log,
                )
                .with_context(|| {
                    format!(
                        "failed to use Apple Virtualization helper {}; ensure it can create or resume Linux VM '{}'",
                        helper.display(),
                        vm_name
                    )
                })?;
            }
        }
        Ok(())
    }

    fn run_shell(&self, script: &str, log: &mut String) -> Result<()> {
        let output = self
            .spawn_shell(script)?
            .output()
            .context("failed to spawn provider shell")?;
        append_process_output(log, &output.stdout, &output.stderr);
        if !output.status.success() {
            let mut message = format!(
                "provider shell command failed with status {}",
                output.status
            );
            append_process_failure_output(&mut message, "stdout", &output.stdout);
            append_process_failure_output(&mut message, "stderr", &output.stderr);
            bail!("{message}");
        }
        Ok(())
    }

    fn push_path(&self, local_path: &Path, remote_parent: &str, log: &mut String) -> Result<()> {
        let local_path = canonical_path(local_path)
            .with_context(|| format!("failed to access {}", local_path.display()))?;
        let parent = local_path
            .parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", local_path.display()))?;
        let name = file_name_string(&local_path)?;
        self.pipe_host_tar_to_provider(
            &[
                "-cf".to_string(),
                "-".to_string(),
                "-C".to_string(),
                parent.display().to_string(),
                name.clone(),
            ],
            &format!(
                "mkdir -p {parent} && tar -xf - -C {parent}",
                parent = shell_quote(remote_parent)
            ),
            log,
        )
    }

    fn pull_path(&self, remote_path: &str, local_parent: &Path, log: &mut String) -> Result<()> {
        let parent = local_parent;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let remote_parent = Path::new(remote_path)
            .parent()
            .ok_or_else(|| anyhow!("remote path has no parent: {remote_path}"))?
            .display()
            .to_string();
        let remote_name = Path::new(remote_path)
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| anyhow!("remote path must have a normal file name: {remote_path}"))?
            .to_string();
        self.pipe_provider_tar_to_host(
            &format!(
                "tar -cf - -C {parent} {name}",
                parent = shell_quote(&remote_parent),
                name = shell_quote(&remote_name),
            ),
            &[
                "-xf".to_string(),
                "-".to_string(),
                "-C".to_string(),
                parent.display().to_string(),
            ],
            log,
        )
    }

    fn pipe_host_tar_to_provider(
        &self,
        tar_args: &[String],
        provider_script: &str,
        log: &mut String,
    ) -> Result<()> {
        log.push_str(&format!(
            "push into provider with tar {}\n",
            tar_args.join(" ")
        ));
        let tar_path = crate::resolve_command_path("tar");
        let mut producer = Command::new(&tar_path);
        producer
            .args(tar_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut consumer = self.spawn_shell(provider_script)?;
        consumer
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        pipe_commands("tar producer", producer, "provider extract", consumer, log)
    }

    fn pipe_provider_tar_to_host(
        &self,
        provider_script: &str,
        tar_args: &[String],
        log: &mut String,
    ) -> Result<()> {
        log.push_str(&format!(
            "pull from provider with tar {}\n",
            tar_args.join(" ")
        ));
        let mut producer = self.spawn_shell(provider_script)?;
        producer.stdout(Stdio::piped()).stderr(Stdio::piped());
        let tar_path = crate::resolve_command_path("tar");
        let mut consumer = Command::new(&tar_path);
        consumer
            .args(tar_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        pipe_commands("provider tar", producer, "host tar", consumer, log)
    }

    fn spawn_shell(&self, script: &str) -> Result<Command> {
        match &self.kind {
            #[cfg(target_os = "windows")]
            ProviderKind::Wsl { distro } => {
                let mut command = Command::new("wsl.exe");
                command.args(["-d", distro, "--", "bash", "-lc", script]);
                Ok(command)
            }
            #[cfg(target_os = "macos")]
            ProviderKind::AppleVirtualization { helper, vm_name } => {
                let mut command = Command::new(helper);
                command.args(["shell", "--vm-name", vm_name, "--script", script]);
                Ok(command)
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn run_host_command<const N: usize>(
        &self,
        executable: &str,
        args: [&str; N],
        log: &mut String,
    ) -> Result<()> {
        let output = self.run_host_capture(executable, args, log)?;
        if !output.status.success() {
            let mut message = format!(
                "host command '{}' failed with status {}",
                executable, output.status
            );
            append_process_failure_output(&mut message, "stdout", &output.stdout);
            append_process_failure_output(&mut message, "stderr", &output.stderr);
            bail!("{message}");
        }
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn run_host_capture<const N: usize>(
        &self,
        executable: &str,
        args: [&str; N],
        log: &mut String,
    ) -> Result<Output> {
        log.push_str(&format!("run host {} {}\n", executable, args.join(" ")));
        let executable_path = crate::resolve_command_path(executable);
        let output = Command::new(&executable_path)
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn {}", executable_path.display()))?;
        append_process_output(log, &output.stdout, &output.stderr);
        Ok(output)
    }

    #[cfg(target_os = "macos")]
    fn run_host_command_vec_path(
        &self,
        executable: &Path,
        args: Vec<String>,
        log: &mut String,
    ) -> Result<()> {
        let output = self.run_host_capture_vec_path(executable, args, log)?;
        if !output.status.success() {
            let mut message = format!(
                "host command '{}' failed with status {}",
                executable.display(),
                output.status
            );
            append_process_failure_output(&mut message, "stdout", &output.stdout);
            append_process_failure_output(&mut message, "stderr", &output.stderr);
            bail!("{message}");
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn run_host_capture_vec_path(
        &self,
        executable: &Path,
        args: Vec<String>,
        log: &mut String,
    ) -> Result<Output> {
        log.push_str(&format!(
            "run host {} {}\n",
            executable.display(),
            args.join(" ")
        ));
        let output = Command::new(executable)
            .args(&args)
            .output()
            .with_context(|| format!("failed to spawn {}", executable.display()))?;
        append_process_output(log, &output.stdout, &output.stderr);
        Ok(output)
    }
}

fn pipe_commands(
    producer_label: &str,
    mut producer: Command,
    consumer_label: &str,
    mut consumer: Command,
    log: &mut String,
) -> Result<()> {
    let mut producer_child = producer
        .spawn()
        .with_context(|| format!("failed to spawn {producer_label}"))?;
    let mut consumer_child = consumer
        .spawn()
        .with_context(|| format!("failed to spawn {consumer_label}"))?;
    {
        let mut producer_stdout = producer_child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("{producer_label} did not expose stdout"))?;
        let mut consumer_stdin = consumer_child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{consumer_label} did not expose stdin"))?;
        io::copy(&mut producer_stdout, &mut consumer_stdin)
            .with_context(|| format!("failed to pipe {producer_label} into {consumer_label}"))?;
    }
    let producer_output = producer_child
        .wait_with_output()
        .with_context(|| format!("failed to wait for {producer_label}"))?;
    let consumer_output = consumer_child
        .wait_with_output()
        .with_context(|| format!("failed to wait for {consumer_label}"))?;
    append_process_output(log, &producer_output.stdout, &producer_output.stderr);
    append_process_output(log, &consumer_output.stdout, &consumer_output.stderr);
    if !producer_output.status.success() {
        let mut message = format!(
            "{producer_label} failed with status {}",
            producer_output.status
        );
        append_process_failure_output(&mut message, "stdout", &producer_output.stdout);
        append_process_failure_output(&mut message, "stderr", &producer_output.stderr);
        bail!("{message}");
    }
    if !consumer_output.status.success() {
        let mut message = format!(
            "{consumer_label} failed with status {}",
            consumer_output.status
        );
        append_process_failure_output(&mut message, "stdout", &consumer_output.stdout);
        append_process_failure_output(&mut message, "stderr", &consumer_output.stderr);
        bail!("{message}");
    }
    Ok(())
}

fn bootstrap_script(runtime_root: &str, repo_root: &str, target_root: &str) -> String {
    format!(
        r#"
set -eu
SUDO=
if [ "$(id -u)" -ne 0 ]; then
  SUDO=sudo
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
cargo build --release --locked --manifest-path {manifest} --target-dir {target}
"#,
        runtime_root = shell_quote(runtime_root),
        bootstrap_stamp = shell_quote(&format!(
            "{runtime_root}/bootstrap-{PROVIDER_BOOTSTRAP_VERSION}.stamp"
        )),
        manifest = shell_quote(&format!("{repo_root}/cli/Cargo.toml")),
        target = shell_quote(target_root),
    )
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

fn configured_provider_runtime_root(source_repo: &Path) -> Result<String> {
    if let Some(raw) = std::env::var_os("DEPOS_LINUX_PROVIDER_ROOT") {
        let raw = raw.to_string_lossy();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("DEPOS_LINUX_PROVIDER_ROOT must be an absolute Linux path, got an empty value");
        }
        if !trimmed.starts_with('/') {
            bail!(
                "DEPOS_LINUX_PROVIDER_ROOT must be an absolute Linux path, got {:?}",
                trimmed
            );
        }
        let normalized = if trimmed == "/" {
            "/".to_string()
        } else {
            trimmed.trim_end_matches('/').to_string()
        };
        return Ok(normalized);
    }

    let runtime_hash = runtime_hash_string(source_repo);
    Ok(format!(
        "/var/tmp/depos-provider/{PROVIDER_RUNTIME_LAYOUT_VERSION}/{runtime_hash}"
    ))
}

fn configured_provider_selection() -> Result<ProviderSelection> {
    let Some(raw) = std::env::var_os("DEPOS_LINUX_PROVIDER") else {
        return Ok(ProviderSelection::Auto);
    };
    let raw = raw.to_string_lossy();
    match raw.trim() {
        "" | "auto" => Ok(ProviderSelection::Auto),
        "wsl2" => Ok(ProviderSelection::Wsl2),
        "mac-local" => Ok(ProviderSelection::MacLocal),
        other => bail!(
            "unsupported DEPOS_LINUX_PROVIDER value {:?}; expected one of: auto, wsl2, mac-local",
            other
        ),
    }
}

fn file_name_string(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(OsStr::to_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("path must have a normal file name: {}", path.display()))
}

fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

#[cfg(target_os = "windows")]
fn detect_wsl_distro() -> Result<String> {
    if let Ok(explicit) = std::env::var("DEPOS_WSL_DISTRO") {
        if !explicit.trim().is_empty() {
            return Ok(explicit);
        }
    }
    let output = Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .output()
        .context("failed to query installed WSL distributions")?;
    if !output.status.success() {
        bail!(
            "wsl.exe --list --quiet failed with status {}",
            output.status
        );
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    listing
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            anyhow!("no WSL distributions were installed; install one or set DEPOS_WSL_DISTRO")
        })
}
