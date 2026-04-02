// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use clap::{Parser, Subcommand};
use depos::{
    collect_statuses, default_depos_root_path, register_depofile, registry_dir_from_manifest,
    sync_registry, unregister_depofile, RegisterOptions, StatusOptions, SyncOptions,
    UnregisterOptions,
};
#[cfg(target_os = "linux")]
use metalor::{run_isolated_container_command, BindMount, ContainerRunCommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "depos")]
#[command(about = "Shared dependency workspace CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    RegistryDir {
        #[arg(long, default_value_os_t = default_depos_root())]
        depos_root: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
    },
    Sync {
        #[arg(long, default_value_os_t = default_depos_root())]
        depos_root: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
    },
    #[cfg(target_os = "linux")]
    #[command(hide = true, name = "internal-run")]
    InternalRun {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        cwd: String,
        #[arg(long = "mount-source")]
        mount_sources: Vec<PathBuf>,
        #[arg(long = "mount-dest")]
        mount_dests: Vec<String>,
        #[arg(long = "mount-mode")]
        mount_modes: Vec<String>,
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(long)]
        emulator: Option<String>,
        #[arg(long)]
        executable: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    Register {
        #[arg(long, default_value_os_t = default_depos_root())]
        depos_root: PathBuf,
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "release")]
        namespace: String,
    },
    Unregister {
        #[arg(long, default_value_os_t = default_depos_root())]
        depos_root: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "release")]
        namespace: String,
        #[arg(long)]
        version: String,
    },
    Status {
        #[arg(long, default_value_os_t = default_depos_root())]
        depos_root: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        refresh: bool,
    },
}

fn default_depos_root() -> PathBuf {
    default_depos_root_path()
}

fn main() -> Result<()> {
    let current_exe = std::env::current_exe()?;
    let cli = Cli::parse();
    match cli.command {
        Command::RegistryDir {
            depos_root,
            manifest,
        } => {
            let registry_dir = registry_dir_from_manifest(&depos_root, &manifest)?;
            println!("{}", registry_dir.display());
        }
        Command::Sync {
            depos_root,
            manifest,
        } => {
            let output = sync_registry(&SyncOptions {
                depos_root,
                manifest,
                executable: Some(current_exe),
            })?;
            println!("{}", output.registry_dir.display());
            for package in output.selected {
                println!("{}", package.spec.package_id());
            }
        }
        #[cfg(target_os = "linux")]
        Command::InternalRun {
            root,
            cwd,
            mount_sources,
            mount_dests,
            mount_modes,
            env,
            emulator,
            executable,
            argv,
        } => {
            if mount_sources.len() != mount_dests.len() || mount_sources.len() != mount_modes.len()
            {
                anyhow::bail!("internal-run mount arguments must have matching counts");
            }
            let mounts = mount_sources
                .into_iter()
                .zip(mount_dests)
                .zip(mount_modes)
                .map(|((source, destination), mode)| {
                    Ok(BindMount {
                        source,
                        destination,
                        read_only: match mode.as_str() {
                            "ro" => true,
                            "rw" => false,
                            _ => return Err(anyhow::anyhow!("unsupported mount mode {}", mode)),
                        },
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let env = env
                .into_iter()
                .map(|entry| {
                    let (key, value) = entry
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("invalid --env '{}'", entry))?;
                    Ok((key.to_string(), value.to_string()))
                })
                .collect::<Result<Vec<_>>>()?;
            run_isolated_container_command(&ContainerRunCommand {
                root,
                cwd,
                mounts,
                env,
                emulator,
                executable,
                argv,
            })?;
        }
        Command::Register {
            depos_root,
            file,
            namespace,
        } => {
            let status = register_depofile(&RegisterOptions {
                depos_root,
                file,
                namespace,
            })?;
            println!("{status}");
        }
        Command::Unregister {
            depos_root,
            name,
            namespace,
            version,
        } => {
            unregister_depofile(&UnregisterOptions {
                depos_root,
                name,
                namespace,
                version,
            })?;
        }
        Command::Status {
            depos_root,
            name,
            namespace,
            version,
            refresh,
        } => {
            let statuses = collect_statuses(&StatusOptions {
                depos_root,
                name,
                namespace,
                version,
                refresh,
            })?;
            for status in statuses {
                println!("{status}");
            }
        }
    }
    Ok(())
}
