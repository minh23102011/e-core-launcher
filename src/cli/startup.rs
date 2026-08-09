use std::error::Error;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use ecore_launcher::{
    resolve_config_path, DirectCommandRunner, IntegrationPaths, RegistryStore, StartupManager,
    StartupStatus,
};

/// Manage launcher-owned graphical-session startup integration.
#[derive(Debug, Args)]
pub struct StartupArgs {
    #[command(subcommand)]
    command: StartupCommand,
}

#[derive(Debug, Subcommand)]
enum StartupCommand {
    /// Install and enable only the user-level supervisor service.
    Enable(StartupEnableArgs),
    /// Disable and remove only launcher-owned integration files.
    Disable(StartupCommonArgs),
    /// Inspect startup state without changing it.
    Status(StartupStatusArgs),
}

#[derive(Clone, Debug, Args)]
struct StartupCommonArgs {
    /// Override XDG_CONFIG_HOME for isolated integration or diagnostics.
    #[arg(long, value_name = "PATH")]
    config_home: Option<PathBuf>,

    /// Override an XDG system config root inspected read-only for autostart.
    #[arg(long, value_name = "PATH", action = clap::ArgAction::Append)]
    config_dir: Vec<PathBuf>,

    /// Override XDG_STATE_HOME for isolated owned-state storage.
    #[arg(long, value_name = "PATH")]
    state_home: Option<PathBuf>,

    /// Direct systemctl executable; no shell is used.
    #[arg(long, value_name = "PATH", default_value = "systemctl")]
    systemctl: PathBuf,
}

#[derive(Debug, Args)]
struct StartupEnableArgs {
    /// Explicitly suppress matching system desktop-autostart entries with owned user overrides.
    #[arg(long)]
    suppress_autostart: bool,

    #[command(flatten)]
    common: StartupCommonArgs,
}

#[derive(Debug, Args)]
struct StartupStatusArgs {
    /// Emit stable machine-readable startup and autostart state.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    common: StartupCommonArgs,
}

pub fn run(arguments: &StartupArgs, config: Option<&Path>) -> Result<(), Box<dyn Error>> {
    match &arguments.command {
        StartupCommand::Enable(enable) => {
            let (manager, store) = manager(&enable.common, config)?;
            let registry = store.load()?;
            let change = manager.enable(&registry, enable.suppress_autostart)?;
            println!("Enabled user startup for the ecore-launcher supervisor.");
            if change.autostart_overrides_changed.is_empty() {
                if enable.suppress_autostart {
                    println!("No matching system autostart entries required new overrides.");
                }
            } else {
                println!(
                    "Suppressed desktop autostart for: {}.",
                    change.autostart_overrides_changed.join(", ")
                );
            }
            println!("No managed application was launched by this command.");
            Ok(())
        }
        StartupCommand::Disable(common) => {
            let (manager, _store) = manager(common, config)?;
            let change = manager.disable()?;
            if change.unit_changed {
                println!("Disabled and removed launcher-owned user startup integration.");
            } else {
                println!("Launcher-owned user startup integration was not installed.");
            }
            if !change.autostart_overrides_changed.is_empty() {
                println!(
                    "Removed launcher-owned autostart overrides for: {}.",
                    change.autostart_overrides_changed.join(", ")
                );
            }
            println!("Running desktop applications and the registry were left untouched.");
            Ok(())
        }
        StartupCommand::Status(status) => {
            let (manager, store) = manager(&status.common, config)?;
            let registry = store.load()?;
            let report = manager.status(&registry)?;
            if status.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_status(&report);
            }
            Ok(())
        }
    }
}

fn manager(
    arguments: &StartupCommonArgs,
    config: Option<&Path>,
) -> Result<(StartupManager<DirectCommandRunner>, RegistryStore), Box<dyn Error>> {
    let paths = IntegrationPaths::from_environment(
        arguments.config_home.as_deref(),
        &arguments.config_dir,
        arguments.state_home.as_deref(),
    )?;
    let registry_path = make_absolute(resolve_config_path(config)?)?;
    let store = RegistryStore::new(&registry_path);
    let manager = StartupManager::new(
        paths,
        std::env::current_exe()?,
        registry_path,
        arguments.systemctl.clone(),
        DirectCommandRunner,
    )?;
    Ok((manager, store))
}

fn make_absolute(path: PathBuf) -> Result<PathBuf, std::io::Error> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn print_status(status: &StartupStatus) {
    println!("User startup integration");
    println!("Unit: {}", status.unit_path.display());
    println!("Present: {}", status.unit_present);
    println!("Launcher-owned: {}", status.unit_owned);
    println!("Current: {}", status.unit_current);
    println!("Ownership marker: {}", status.ownership_owned);
    println!(
        "Enabled: {}",
        status
            .enabled
            .map_or("unknown", |enabled| if enabled { "yes" } else { "no" })
    );
    if let Some(diagnostic) = &status.systemctl_diagnostic {
        println!("systemctl diagnostic: {diagnostic}");
    }
    if let Some(environment) = &status.manager_environment {
        println!(
            "User-manager graphical environment: {}",
            environment.is_ready()
        );
    } else if let Some(diagnostic) = &status.manager_environment_diagnostic {
        println!("User-manager environment diagnostic: {diagnostic}");
    }
    if status.autostart.is_empty() {
        println!("Autostart: no enabled registered applications.");
    } else {
        println!("Autostart:");
        for assessment in &status.autostart {
            println!("  {}: {:?}", assessment.desktop_id, assessment.state);
        }
    }
}
