use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;

use n10e::bench::{run_bench, seed_synthetic_corpus};
use n10e::config::AppConfig;
use n10e::constants::PROJECT_CODENAME;
use n10e::dev::run_dev_supervisor;
use n10e::doctor::run_doctor;
use n10e::http::{WebState, router};
use n10e::mcp::{run_stdio_server_forever, spawn_stdio_server};
use n10e::service::AppService;

#[derive(Debug, Parser)]
#[command(name = "n10e")]
#[command(version)]
#[command(about = "Agent-native local PM + knowledge hub")]
struct Cli {
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Init {
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    Serve,
    Dev,
    Reindex,
    Import {
        #[arg(value_name = "SOURCE_PATH")]
        path: PathBuf,
    },
    Doctor,
    Bench {
        #[arg(long, default_value_t = 200)]
        iterations: usize,
        #[arg(long, default_value = "task")]
        query: String,
    },
    SeedBench {
        #[arg(long, default_value_t = 5_000)]
        count: usize,
    },
    Mcp,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { path } => cmd_init(path),
        Commands::Serve => cmd_serve(cli.workspace).await,
        Commands::Dev => run_dev_supervisor(cli.workspace).await,
        Commands::Reindex => cmd_reindex(cli.workspace),
        Commands::Import { path } => cmd_import(cli.workspace, path),
        Commands::Doctor => cmd_doctor(cli.workspace),
        Commands::Bench { iterations, query } => cmd_bench(cli.workspace, iterations, &query),
        Commands::SeedBench { count } => cmd_seed_bench(cli.workspace, count),
        Commands::Mcp => cmd_mcp(cli.workspace).await,
    }
}

fn cmd_init(path: Option<PathBuf>) -> Result<()> {
    let workspace_root = resolve_workspace(path)?;
    std::fs::create_dir_all(&workspace_root)
        .with_context(|| format!("failed creating workspace {}", workspace_root.display()))?;

    let mut config = AppConfig::default_for_workspace(workspace_root.clone());
    config.codename = PROJECT_CODENAME.to_string();
    config.ensure_workspace_dirs()?;
    let path = config.save_to_workspace(&workspace_root)?;

    let _service = AppService::bootstrap(config.clone())?;

    println!("Initialized n10e workspace at {}", workspace_root.display());
    println!("Config: {}", path.display());
    println!("Codename: {}", config.codename);
    Ok(())
}

async fn cmd_serve(workspace: Option<PathBuf>) -> Result<()> {
    let config = load_config(workspace)?;
    let service = Arc::new(AppService::bootstrap(config.clone())?);
    let _watcher = service.start_watcher()?;
    let _mcp = spawn_stdio_server(service.clone());

    let state = Arc::new(WebState {
        service,
        dev_reload_token: std::env::var("N10E_DEV_RELOAD_TOKEN").ok(),
    });

    let app = router(state);

    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .context("invalid server host/port configuration")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed binding http listener at {addr}"))?;

    info!(address = %addr, "n10e server starting");
    println!("n10e serve listening on http://{addr}");

    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

fn cmd_reindex(workspace: Option<PathBuf>) -> Result<()> {
    let mut config = load_config(workspace)?;
    config.index.startup_full_scan = false;
    let service = AppService::bootstrap(config)?;
    service.reindex_all()?;
    println!("Reindex completed.");
    Ok(())
}

fn cmd_import(workspace: Option<PathBuf>, source_path: PathBuf) -> Result<()> {
    let mut config = load_config(workspace)?;
    config.index.startup_full_scan = false;
    let service = AppService::bootstrap(config)?;
    let imported = service.import_tree(&source_path)?;
    println!(
        "Imported {} markdown files from {}",
        imported,
        source_path.display()
    );
    Ok(())
}

fn cmd_doctor(workspace: Option<PathBuf>) -> Result<()> {
    let config = load_config(workspace)?;
    let report = run_doctor(&config)?;

    println!("doctor::config_ok={}", report.config_ok);
    println!("doctor::workspace_ok={}", report.workspace_ok);
    println!("doctor::db_ok={}", report.db_ok);
    println!("doctor::watcher_ok={}", report.watcher_ok);
    println!("doctor::write_boundary_ok={}", report.write_boundary_ok);

    if report.healthy() {
        println!("Doctor report: healthy");
        Ok(())
    } else {
        anyhow::bail!("doctor report: unhealthy")
    }
}

fn cmd_bench(workspace: Option<PathBuf>, iterations: usize, query: &str) -> Result<()> {
    let mut config = load_config(workspace)?;
    config.index.startup_full_scan = true;
    let service = AppService::bootstrap(config)?;
    let report = run_bench(&service, iterations, query)?;

    println!("benchmark::query={}", report.query);
    println!("benchmark::iterations={}", report.iterations);
    println!("benchmark::search_p50_ms={:.3}", report.search_p50_ms);
    println!("benchmark::search_p95_ms={:.3}", report.search_p95_ms);
    println!("benchmark::read_p50_ms={:.3}", report.read_p50_ms);
    println!("benchmark::read_p95_ms={:.3}", report.read_p95_ms);

    Ok(())
}

fn cmd_seed_bench(workspace: Option<PathBuf>, count: usize) -> Result<()> {
    let mut config = load_config(workspace)?;
    config.index.startup_full_scan = false;
    let service = AppService::bootstrap(config)?;
    let report = seed_synthetic_corpus(&service, count)?;
    println!("seed_bench::requested={}", report.requested);
    println!("seed_bench::created={}", report.created);
    println!("seed_bench::corpus_dir={}", report.corpus_dir);
    Ok(())
}

async fn cmd_mcp(workspace: Option<PathBuf>) -> Result<()> {
    let config = load_config(workspace)?;
    let service = Arc::new(AppService::bootstrap(config)?);
    let _watcher = service.start_watcher()?;
    run_stdio_server_forever(service).await
}

fn load_config(workspace_override: Option<PathBuf>) -> Result<AppConfig> {
    let workspace_root = resolve_workspace(workspace_override)?;
    AppConfig::load_from_workspace(&workspace_root)
}

fn resolve_workspace(path: Option<PathBuf>) -> Result<PathBuf> {
    let root = match path {
        Some(path) => path,
        None => std::env::current_dir().context("failed reading current working directory")?,
    };

    if root.is_absolute() {
        Ok(root)
    } else {
        Ok(std::env::current_dir()?.join(root))
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "n10e=info,axum=info".into()),
        )
        .with_target(false)
        .compact()
        .try_init();
}
