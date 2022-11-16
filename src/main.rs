mod config;
mod run;
pub mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use run::{cargo, reload, sass, serve, wasm_pack, watch, Html};
use std::env;
use tokio::{
    signal,
    sync::{broadcast, RwLock},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// sent by ctrl-c
    ShutDown,
    /// sent by fs watcher
    SrcChanged,
    /// messages sent to reload server (forwarded to browser)
    Reload(String),
}

lazy_static::lazy_static! {
    /// Interrupts current serve or cargo operation. Used for watch
    pub static ref MSG_BUS: broadcast::Sender<Msg> = {
        broadcast::channel(10).0
    };
    pub static ref SHUTDOWN: RwLock<bool> = RwLock::new(false);
}

#[derive(Debug, Clone, Parser, PartialEq, Default)]
pub struct Opts {
    /// Build artifacts in release mode, with optimizations
    #[arg(short, long)]
    release: bool,

    /// Build for client side rendering. Useful during development due to faster compile times.
    #[arg(long)]
    csr: bool,

    /// Verbosity (none: errors & warnings, -v: verbose, --vv: very verbose, --vvv: output everything)
    #[arg(short, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug, Parser)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand, PartialEq)]
enum Commands {
    /// Adds a default leptos.toml file to current directory
    Init,
    /// Compile the client (csr and hydrate) and server
    Build(Opts),
    /// Run the cargo tests for app, client and server
    Test(Opts),
    /// Serve. In `csr` mode an internal server is used
    Serve(Opts),
    /// Serve and automatically reload when files change
    Watch(Opts),
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args: Vec<String> = env::args().collect();
    // when running as cargo leptos, the second argument is "leptos" which
    // clap doesn't expect
    if args.get(1).map(|a| a == "leptos").unwrap_or(false) {
        args.remove(1);
    }

    let args = Cli::parse_from(&args);

    let opts = match &args.command {
        Commands::Init => return config::save_default_file(),
        Commands::Build(opts)
        | Commands::Serve(opts)
        | Commands::Test(opts)
        | Commands::Watch(opts) => opts,
    };
    util::setup_logging(opts.verbose);

    let config = config::read(&args, opts.clone())?;

    tokio::spawn(async {
        signal::ctrl_c().await.expect("failed to listen for event");
        log::info!("Ctrl-c received");
        *SHUTDOWN.write().await = true;
        MSG_BUS.send(Msg::ShutDown).unwrap();
    });

    match args.command {
        Commands::Init => panic!(),
        Commands::Build(_) => build_all(&config).await,
        Commands::Serve(_) => serve(&config).await,
        Commands::Test(_) => cargo::test(&config).await,
        Commands::Watch(_) => watch(&config).await,
    }
}

async fn send_reload() {
    if !*SHUTDOWN.read().await {
        if let Err(e) = MSG_BUS.send(Msg::Reload("reload".to_string())) {
            log::error!("Failed to send reload: {e}");
        }
    }
}
async fn build_csr_or_ssr(config: &Config) -> Result<()> {
    util::rm_dir_content("target/site")?;
    build_client(&config).await?;

    if !config.cli.csr {
        cargo::build(&config).await?;
    }
    Ok(())
}
async fn build_client(config: &Config) -> Result<()> {
    sass::run(&config).await?;

    let html = Html::read(&config.index_path)?;

    if config.cli.csr {
        wasm_pack::build(&config).await?;
        html.generate_html(&config)?;
    } else {
        wasm_pack::build(&config).await?;
        html.generate_rust(&config)?;
    }
    Ok(())
}

async fn build_all(config: &Config) -> Result<()> {
    util::rm_dir_content("target/site")?;

    cargo::build(&config).await?;
    sass::run(&config).await?;

    let html = Html::read(&config.index_path)?;

    html.generate_html(&config)?;
    html.generate_rust(&config)?;

    let mut config = config.clone();

    config.cli.csr = true;
    wasm_pack::build(&config).await?;
    config.cli.csr = false;
    wasm_pack::build(&config).await?;
    Ok(())
}

async fn serve(config: &Config) -> Result<()> {
    build_csr_or_ssr(&config).await?;
    if config.cli.csr {
        serve::run(&config).await
    } else {
        cargo::run(&config).await
    }
}

async fn watch(config: &Config) -> Result<()> {
    let cfg = config.clone();
    let _ = tokio::spawn(async move { watch::run(cfg).await });

    if config.cli.csr {
        let cfg = config.clone();
        let _ = tokio::spawn(async move { serve::run(&cfg).await });
    }

    reload::run(&config).await?;

    loop {
        build_csr_or_ssr(config).await?;

        send_reload().await;
        if config.cli.csr {
            MSG_BUS.subscribe().recv().await?;
        } else {
            cargo::run(&config).await?;
        }
        if *SHUTDOWN.read().await {
            break;
        }
    }
    Ok(())
}
