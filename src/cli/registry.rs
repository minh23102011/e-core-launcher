use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use ecore_launcher::{
    resolve_config_path, AppRegistry, ApplicationSettingsUpdate, DesktopApplicationScanner,
    DiscoveryReport, IoPriorityClass, RegisteredApplicationAvailability,
    RegisteredApplicationStatus, RegistryError, RegistryMutationResult, RegistryStore,
};
use serde::Serialize;

use super::discover::DiscoveryArgs;

#[derive(Debug, Subcommand)]
pub enum RegistryCommand {
    /// Explicitly select discovered desktop applications for later management.
    Add(AddArgs),
    /// List explicitly selected applications without launching them.
    List(ListArgs),
    /// Show one explicit registry entry and optional current availability.
    Show(ShowArgs),
    /// Remove explicit registry entries without changing desktop files or processes.
    Remove(RemoveArgs),
    /// Mark explicit registry entries enabled for launching.
    Enable(IdsArgs),
    /// Mark explicit registry entries disabled for launching.
    Disable(IdsArgs),
    /// Update validated stored launch preferences.
    Configure(ConfigureArgs),
    /// Inspect the resolved registry configuration.
    Config(ConfigArgs),
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Stable desktop IDs to add non-interactively. Omit for terminal selection.
    #[arg(value_name = "DESKTOP_ID")]
    desktop_ids: Vec<String>,

    #[command(flatten)]
    discovery: DiscoveryArgs,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit stable JSON containing registry, availability, and warnings.
    #[arg(long)]
    json: bool,

    /// Re-run discovery to report currently available and unavailable IDs.
    #[arg(long)]
    check_availability: bool,

    #[command(flatten)]
    discovery: DiscoveryArgs,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Registered stable desktop ID.
    desktop_id: String,

    /// Emit stable JSON.
    #[arg(long)]
    json: bool,

    /// Re-run discovery to report current availability.
    #[arg(long)]
    check_availability: bool,

    #[command(flatten)]
    discovery: DiscoveryArgs,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Registered stable desktop IDs to remove.
    #[arg(value_name = "DESKTOP_ID", required = true)]
    desktop_ids: Vec<String>,

    /// Confirm removal without terminal interaction.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
pub struct IdsArgs {
    /// Registered stable desktop IDs to update.
    #[arg(value_name = "DESKTOP_ID", required = true)]
    desktop_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum IoClassArg {
    None,
    Realtime,
    BestEffort,
    Idle,
}

impl From<IoClassArg> for IoPriorityClass {
    fn from(value: IoClassArg) -> Self {
        match value {
            IoClassArg::None => Self::None,
            IoClassArg::Realtime => Self::Realtime,
            IoClassArg::BestEffort => Self::BestEffort,
            IoClassArg::Idle => Self::Idle,
        }
    }
}

#[derive(Debug, Args)]
pub struct ConfigureArgs {
    /// Registered stable desktop ID.
    desktop_id: String,

    /// Startup delay in seconds (0 through 3600).
    #[arg(long)]
    delay: Option<u64>,

    /// Linux nice value (-20 through 19; negative values may require privileges).
    #[arg(long)]
    nice: Option<i8>,

    /// Linux I/O scheduling class applied at launch.
    #[arg(long, value_enum)]
    io_class: Option<IoClassArg>,

    /// I/O priority (0 through 7 for best-effort or realtime).
    #[arg(long)]
    io_priority: Option<u8>,

    /// Whether supervised launches should enforce descendant affinity.
    #[arg(long, value_name = "BOOL")]
    enforce_process_tree: Option<bool>,

    /// Restore all stored policy fields from current launcher defaults.
    #[arg(long)]
    reset: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the resolved configuration path without creating it.
    Path,
    /// Load and validate the configuration without modifying it.
    Validate,
    /// Print the parsed configuration without modifying it.
    Show(ConfigShowArgs),
}

#[derive(Debug, Args)]
struct ConfigShowArgs {
    /// Emit stable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Serialize)]
struct RegistryListOutput {
    config_path: PathBuf,
    schema_version: u32,
    launcher: ecore_launcher::LauncherDefaults,
    applications: Vec<RegisteredApplicationStatus>,
    warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct RegistryShowOutput {
    config_path: PathBuf,
    schema_version: u32,
    application: RegisteredApplicationStatus,
}

pub fn run(command: &RegistryCommand, config: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let path = resolve_config_path(config)?;
    let store = RegistryStore::new(path);
    match command {
        RegistryCommand::Add(arguments) => add(&store, arguments),
        RegistryCommand::List(arguments) => list(&store, arguments),
        RegistryCommand::Show(arguments) => show(&store, arguments),
        RegistryCommand::Remove(arguments) => remove(&store, arguments),
        RegistryCommand::Enable(arguments) => set_enabled(&store, arguments, true),
        RegistryCommand::Disable(arguments) => set_enabled(&store, arguments, false),
        RegistryCommand::Configure(arguments) => configure(&store, arguments),
        RegistryCommand::Config(arguments) => config_command(&store, arguments),
    }
}

fn add(store: &RegistryStore, arguments: &AddArgs) -> Result<(), Box<dyn Error>> {
    let discovery = discover(&arguments.discovery)?;
    let selected = if arguments.desktop_ids.is_empty() {
        interactive_selection(&discovery, &store.load()?)?
    } else {
        select_ids(&discovery, &arguments.desktop_ids)?
    };
    let result = store.mutate(|registry| registry.add_discovered(&selected))?;
    if result.added.is_empty() {
        println!("No new applications were added.");
    } else {
        println!(
            "Added {} application{}.",
            result.added.len(),
            plural(result.added.len())
        );
        for desktop_id in &result.added {
            println!("  added: {desktop_id}");
        }
    }
    for desktop_id in &result.already_registered {
        println!("  already registered: {desktop_id}");
    }
    Ok(())
}

fn list(store: &RegistryStore, arguments: &ListArgs) -> Result<(), Box<dyn Error>> {
    let registry = store.load()?;
    let discovery = arguments
        .check_availability
        .then(|| discover(&arguments.discovery))
        .transpose()?;
    let output = RegistryListOutput {
        config_path: store.path().to_owned(),
        schema_version: registry.schema_version,
        launcher: registry.launcher.clone(),
        applications: registry.resolve_against(discovery.as_ref()),
        warnings: discovery
            .as_ref()
            .map(|report| {
                report
                    .warnings
                    .iter()
                    .map(|warning| format!("{}: {}", warning.path.display(), warning.reason))
                    .collect()
            })
            .unwrap_or_default(),
    };
    if arguments.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_list(&output);
    }
    Ok(())
}

fn show(store: &RegistryStore, arguments: &ShowArgs) -> Result<(), Box<dyn Error>> {
    let registry = store.load()?;
    let discovery = arguments
        .check_availability
        .then(|| discover(&arguments.discovery))
        .transpose()?;
    let application = registry
        .resolve_against(discovery.as_ref())
        .into_iter()
        .find(|status| status.application.desktop_id == arguments.desktop_id)
        .ok_or_else(|| RegistryError::UnknownRegisteredApplication {
            desktop_id: arguments.desktop_id.clone(),
        })?;
    let output = RegistryShowOutput {
        config_path: store.path().to_owned(),
        schema_version: registry.schema_version,
        application,
    };
    if arguments.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_show(&output);
    }
    Ok(())
}

fn remove(store: &RegistryStore, arguments: &RemoveArgs) -> Result<(), Box<dyn Error>> {
    if !arguments.yes {
        confirm_removal(&arguments.desktop_ids)?;
    }
    let result = store.mutate(|registry| registry.remove(&arguments.desktop_ids))?;
    print_mutation("Removed", &result);
    Ok(())
}

fn set_enabled(
    store: &RegistryStore,
    arguments: &IdsArgs,
    enabled: bool,
) -> Result<(), Box<dyn Error>> {
    let result = store.mutate(|registry| registry.set_enabled(&arguments.desktop_ids, enabled))?;
    print_mutation(if enabled { "Enabled" } else { "Disabled" }, &result);
    Ok(())
}

fn configure(store: &RegistryStore, arguments: &ConfigureArgs) -> Result<(), Box<dyn Error>> {
    let update = ApplicationSettingsUpdate {
        delay_seconds: arguments.delay,
        nice: arguments.nice,
        io_class: arguments.io_class.map(Into::into),
        io_priority: arguments.io_priority.map(Some),
        enforce_process_tree: arguments.enforce_process_tree,
        reset_to_defaults: arguments.reset,
    };
    let changed = store.mutate(|registry| registry.configure(&arguments.desktop_id, &update))?;
    if changed {
        println!("Configured {}.", arguments.desktop_id);
    } else {
        println!(
            "{} already had the requested settings.",
            arguments.desktop_id
        );
    }
    Ok(())
}

fn config_command(store: &RegistryStore, arguments: &ConfigArgs) -> Result<(), Box<dyn Error>> {
    match &arguments.command {
        ConfigCommand::Path => println!("{}", store.path().display()),
        ConfigCommand::Validate => {
            let load = store.load_with_status()?;
            if load.exists {
                println!("Configuration {} is valid.", store.path().display());
            } else {
                println!(
                    "Configuration {} does not exist; the empty default registry is valid.",
                    store.path().display()
                );
            }
        }
        ConfigCommand::Show(show) => {
            let load = store.load_with_status()?;
            let output = RegistryListOutput {
                config_path: store.path().to_owned(),
                schema_version: load.registry.schema_version,
                launcher: load.registry.launcher.clone(),
                applications: load.registry.resolve_against(None),
                warnings: Vec::new(),
            };
            if show.json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                print_list(&output);
            }
        }
    }
    Ok(())
}

fn discover(arguments: &DiscoveryArgs) -> Result<DiscoveryReport, Box<dyn Error>> {
    Ok(DesktopApplicationScanner::from_options(arguments.options()).discover()?)
}

fn select_ids(
    report: &DiscoveryReport,
    requested: &[String],
) -> Result<Vec<ecore_launcher::DiscoveredApplication>, RegistryError> {
    let by_id: BTreeMap<&str, &ecore_launcher::DiscoveredApplication> = report
        .applications
        .iter()
        .map(|application| (application.desktop_id.as_str(), application))
        .collect();
    let requested: BTreeSet<&str> = requested.iter().map(String::as_str).collect();
    let mut selected = Vec::new();
    for desktop_id in requested {
        let application =
            by_id
                .get(desktop_id)
                .ok_or_else(|| RegistryError::UnknownDiscoveredApplication {
                    desktop_id: desktop_id.to_owned(),
                })?;
        selected.push((*application).clone());
    }
    Ok(selected)
}

fn interactive_selection(
    report: &DiscoveryReport,
    registry: &AppRegistry,
) -> Result<Vec<ecore_launcher::DiscoveredApplication>, RegistryError> {
    if !io::stdin().is_terminal() {
        return Err(RegistryError::InteractiveInputUnavailable);
    }
    if report.applications.is_empty() {
        return Err(RegistryError::InteractiveCanceled);
    }
    let registered: BTreeSet<&str> = registry
        .apps
        .iter()
        .map(|application| application.desktop_id.as_str())
        .collect();
    println!("Select applications to manage (comma-separated numbers; blank cancels):\n");
    for (index, application) in report.applications.iter().enumerate() {
        let marker = if registered.contains(application.desktop_id.as_str()) {
            "registered"
        } else {
            " "
        };
        println!(
            "{:>3}. [{}] {} ({})",
            index + 1,
            marker,
            application.name,
            application.desktop_id
        );
    }
    print!("Selection: ");
    io::stdout()
        .flush()
        .map_err(|source| RegistryError::InteractiveIo {
            operation: "stdout flush",
            source,
        })?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|source| RegistryError::InteractiveIo {
            operation: "stdin read",
            source,
        })?;
    let input = input.trim();
    if input.is_empty() {
        return Err(RegistryError::InteractiveCanceled);
    }
    let mut indexes = BTreeSet::new();
    for value in input
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let index = value
            .parse::<usize>()
            .ok()
            .filter(|index| (1..=report.applications.len()).contains(index))
            .ok_or_else(|| RegistryError::InvalidInteractiveSelection {
                value: value.to_owned(),
            })?;
        indexes.insert(index - 1);
    }
    if indexes.is_empty() {
        return Err(RegistryError::InteractiveCanceled);
    }
    Ok(indexes
        .into_iter()
        .filter_map(|index| report.applications.get(index).cloned())
        .collect())
}

fn confirm_removal(desktop_ids: &[String]) -> Result<(), RegistryError> {
    if !io::stdin().is_terminal() {
        return Err(RegistryError::InteractiveInputUnavailable);
    }
    println!(
        "Remove {} application{}? [y/N]",
        desktop_ids.len(),
        plural(desktop_ids.len())
    );
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|source| RegistryError::InteractiveIo {
            operation: "stdin read",
            source,
        })?;
    if matches!(input.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(RegistryError::InteractiveCanceled)
    }
}

fn print_list(output: &RegistryListOutput) {
    println!("Managed applications\n");
    if output.applications.is_empty() {
        println!("No applications are registered.");
    } else {
        println!("STATE     AVAILABILITY  ID  APPLICATION  DELAY  NICE");
        for status in &output.applications {
            let application = &status.application;
            let state = if application.enabled {
                "enabled"
            } else {
                "disabled"
            };
            let availability = match status.availability {
                RegisteredApplicationAvailability::Available { .. } => "available",
                RegisteredApplicationAvailability::Unavailable => "unavailable",
                RegisteredApplicationAvailability::Unknown => "unchecked",
            };
            println!(
                "{state:<9} {availability:<13} {}  {}  {}s  {}",
                application.desktop_id,
                application.name,
                application.delay_seconds,
                application.nice
            );
        }
    }
    println!(
        "\n{} registered application{}.",
        output.applications.len(),
        plural(output.applications.len())
    );
}

fn print_show(output: &RegistryShowOutput) {
    let application = &output.application.application;
    println!("Managed application\n");
    println!("ID:                   {}", application.desktop_id);
    println!("Name:                 {}", application.name);
    println!("Enabled:              {}", application.enabled);
    println!("Delay:                {}s", application.delay_seconds);
    println!("Nice:                 {}", application.nice);
    println!("I/O class:            {}", application.io_class);
    println!(
        "I/O priority:         {}",
        application
            .io_priority
            .map_or_else(|| "not applicable".to_owned(), |value| value.to_string())
    );
    println!("Process tree:         {}", application.enforce_process_tree);
    println!(
        "Desktop file snapshot: {}",
        application.desktop_file.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |path| path.display().to_string()
        )
    );
    println!(
        "Availability:         {}",
        availability_label(&output.application.availability)
    );
}

fn print_mutation(action: &str, result: &RegistryMutationResult) {
    println!(
        "{action} {} application{}.",
        result.changed.len(),
        plural(result.changed.len())
    );
    for desktop_id in &result.changed {
        println!("  changed: {desktop_id}");
    }
    for desktop_id in &result.unchanged {
        println!("  unchanged: {desktop_id}");
    }
}

fn availability_label(availability: &RegisteredApplicationAvailability) -> String {
    match availability {
        RegisteredApplicationAvailability::Available { current_name } => {
            format!("available ({current_name})")
        }
        RegisteredApplicationAvailability::Unavailable => "unavailable".to_owned(),
        RegisteredApplicationAvailability::Unknown => "not checked".to_owned(),
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}
