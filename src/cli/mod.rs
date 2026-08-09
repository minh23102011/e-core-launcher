mod discover;
mod registry;
mod run;
mod topology;

use std::error::Error;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use self::discover::DiscoverArgs;
use self::run::{HelperArgs, RunArgs};
use self::topology::TopologyArgs;

#[derive(Debug, Parser)]
#[command(
    name = "ecore-launcher",
    version,
    about = "Opt-in Linux desktop application launcher for reliably detected E-cores"
)]
struct Cli {
    /// Override the XDG registry configuration file for registry commands.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Explicitly select discovered desktop applications for later management.
    Add(registry::AddArgs),
    /// List explicitly selected applications without launching them.
    List(registry::ListArgs),
    /// Show one explicit registry entry.
    Show(registry::ShowArgs),
    /// Remove explicit registry entries without changing desktop files or processes.
    Remove(registry::RemoveArgs),
    /// Mark explicit registry entries enabled for a later launcher phase.
    Enable(registry::IdsArgs),
    /// Mark explicit registry entries disabled for a later launcher phase.
    Disable(registry::IdsArgs),
    /// Update validated stored preferences for a later launcher phase.
    Configure(registry::ConfigureArgs),
    /// Inspect the resolved registry configuration.
    Config(registry::ConfigArgs),
    /// Discover usable installed desktop applications without launching them.
    Discover(DiscoverArgs),
    /// Inspect active CPU topology without modifying the system.
    Topology(TopologyArgs),
    /// Launch enabled, explicitly registered applications on detected E-cores.
    Run(RunArgs),
    #[command(name = "__exec", hide = true)]
    InternalExec(HelperArgs),
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Add(arguments) => registry::run(
            &registry::RegistryCommand::Add(arguments),
            cli.config.as_deref(),
        ),
        Command::List(arguments) => registry::run(
            &registry::RegistryCommand::List(arguments),
            cli.config.as_deref(),
        ),
        Command::Show(arguments) => registry::run(
            &registry::RegistryCommand::Show(arguments),
            cli.config.as_deref(),
        ),
        Command::Remove(arguments) => registry::run(
            &registry::RegistryCommand::Remove(arguments),
            cli.config.as_deref(),
        ),
        Command::Enable(arguments) => registry::run(
            &registry::RegistryCommand::Enable(arguments),
            cli.config.as_deref(),
        ),
        Command::Disable(arguments) => registry::run(
            &registry::RegistryCommand::Disable(arguments),
            cli.config.as_deref(),
        ),
        Command::Configure(arguments) => registry::run(
            &registry::RegistryCommand::Configure(arguments),
            cli.config.as_deref(),
        ),
        Command::Config(arguments) => registry::run(
            &registry::RegistryCommand::Config(arguments),
            cli.config.as_deref(),
        ),
        Command::Discover(arguments) => discover::run(&arguments),
        Command::Topology(arguments) => topology::run(&arguments),
        Command::Run(arguments) => run::run(&arguments, cli.config.as_deref()),
        Command::InternalExec(arguments) => run::run_helper(&arguments),
    }
}
