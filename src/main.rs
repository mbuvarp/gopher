use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gopher::{config::Config, github::Github, logging, reviewers};

#[derive(Parser)]
#[command(version, about = "GitHub agent reviews in your macOS menu bar")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Subcommand)]
enum Commands {
    /// Validate configuration and GitHub authentication without starting the app.
    Doctor,
    /// Print live review evidence as JSON; does not write PR state or send notifications.
    Inspect { repo: String, number: u64 },
}

fn main() -> Result<()> {
    let args = Cli::parse();
    let directory = Config::directory()?;
    let config = Config::load(&directory)?;
    if let Some(command) = args.command {
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(async {
            let github=Github::new(&config)?;
            match command {
                Commands::Doctor=>{
                    let login=github.viewer().await?;
                    println!("GitHub CLI: {}\nAuthenticated as: {login}\nData directory: {}\nPolling: {} seconds",gopher::github::resolve_gh(&config)?.display(),directory.display(),config.poll_seconds);
                }
                Commands::Inspect{repo,number}=>{
                    let reference=github.resolve_pr(&repo,number).await?;
                    let snapshot=github.snapshot(&reference).await?;
                    let expected=config.repositories.get(&repo).and_then(|r|r.reviewers.as_deref());
                    let agents=reviewers::evaluate(&snapshot,None,expected);
                    let state=reviewers::aggregate(&snapshot,&agents);
                    println!("{}",serde_json::to_string_pretty(&serde_json::json!({"repo":repo,"number":number,"head":snapshot.head,"open":snapshot.open,"state":state,"agents":agents,"unresolved_threads":snapshot.threads.iter().filter(|t|!t.resolved).count(),"note":"Live evidence only; no cached run history or settling interval."}))?);
                }
            }
            Ok(())
        });
    }
    std::fs::create_dir_all(&directory)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("gopher.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("Gopher is already running")?;
    let (writer, _guard) = logging::start(&directory.join("logs"))?;
    tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&config.log_level)?)
        .with_writer(writer)
        .init();
    tracing::info!(event = "app_started", version = env!("CARGO_PKG_VERSION"));
    #[cfg(target_os = "macos")]
    gopher::macos::run(directory, config)?;
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("The menu bar app requires macOS; use doctor or inspect for diagnostics");
    Ok(())
}
