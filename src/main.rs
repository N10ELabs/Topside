use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::task::JoinHandle;
use tracing::info;

use topside::bench::{run_bench, seed_synthetic_corpus};
use topside::bundle::bundle_macos_app;
use topside::config::{AppConfig, maybe_migrate_workspace_identity};
use topside::constants::{CONFIG_FILE_NAME, PROJECT_CODENAME};
use topside::desktop::{run_native_window, window_title};
use topside::dev::run_dev_supervisor;
use topside::doctor::run_doctor;
use topside::http::{WebState, router};
use topside::mcp::run_stdio_server_forever;
use topside::service::AppService;

#[derive(Debug, Parser)]
#[command(name = "topside")]
#[command(version)]
#[command(about = "Topside: local-first project context hub")]
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
    Open,
    BundleApp {
        #[arg(long, value_name = "DIR")]
        output_dir: Option<PathBuf>,
        #[arg(long, value_name = "FILE")]
        icon: Option<PathBuf>,
    },
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
        Commands::Open => cmd_open(cli.workspace).await,
        Commands::BundleApp { output_dir, icon } => cmd_bundle_app(cli.workspace, output_dir, icon),
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

    maybe_migrate_workspace_identity(&workspace_root)?;

    let config_path = workspace_root.join(CONFIG_FILE_NAME);
    let config = if config_path.exists() {
        let mut config = AppConfig::load_from_workspace(&workspace_root)?;
        config.workspace_root = workspace_root.clone();
        config
    } else {
        let mut config = AppConfig::default_for_workspace(workspace_root.clone());
        config.codename = PROJECT_CODENAME.to_string();
        config
    };
    config.ensure_workspace_dirs()?;
    let path = config.save_to_workspace(&workspace_root)?;

    let _service = AppService::bootstrap(config.clone())?;

    println!(
        "Initialized Topside workspace at {}",
        workspace_root.display()
    );
    println!("Config: {}", path.display());
    println!("Codename: {}", config.codename);
    Ok(())
}

async fn cmd_serve(workspace: Option<PathBuf>) -> Result<()> {
    let config = load_config(workspace)?;
    let service = Arc::new(AppService::bootstrap(config.clone())?);
    let _watcher = service.start_watcher()?;
    let app = router(build_web_state(service));
    let (addr, listener) = bind_http_listener(&config).await?;

    info!(address = %addr, "topside server starting");
    println!("topside serve listening on http://{addr}");

    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

async fn cmd_open(workspace: Option<PathBuf>) -> Result<()> {
    let config = load_config(workspace)?;
    let service = Arc::new(AppService::bootstrap(config.clone())?);
    let _watcher = service.start_watcher()?;
    let app = router(build_web_state(service));
    let (addr, listener) = bind_http_listener(&config).await?;
    let _http_task = spawn_http_server(listener, app);
    let url = format!("http://{addr}");
    let title = window_title(&config.workspace_root);

    info!(address = %addr, "topside desktop window starting");
    println!("topside open launching native window at {url}");

    run_native_window(&url, &title, &config.workspace_root)
}

fn cmd_bundle_app(
    workspace: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    icon: Option<PathBuf>,
) -> Result<()> {
    let default_workspace = workspace
        .map(|path| resolve_workspace(Some(path)))
        .transpose()?;
    let source_binary = std::env::current_exe().context("failed locating current executable")?;
    let output_dir = resolve_output_dir(output_dir)?;
    let icon = icon.map(resolve_absolute_path).transpose()?;
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed creating {}", output_dir.display()))?;

    let bundle_path = bundle_macos_app(
        &source_binary,
        &output_dir,
        default_workspace.as_deref(),
        icon.as_deref(),
    )?;

    println!("Created macOS app bundle at {}", bundle_path.display());
    if let Some(workspace) = default_workspace {
        println!("Default workspace: {}", workspace.display());
    } else {
        println!("Default workspace: prompt on launch");
    }
    if let Some(icon) = icon {
        println!("Bundle icon: {}", icon.display());
    } else {
        println!("Bundle icon: none");
    }

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
    let _ = load_config(workspace)?;
    run_stdio_server_forever().await
}

fn load_config(workspace_override: Option<PathBuf>) -> Result<AppConfig> {
    let workspace_root = resolve_workspace(workspace_override)?;
    AppConfig::load_from_workspace(&workspace_root)
}

fn build_web_state(service: Arc<AppService>) -> Arc<WebState> {
    Arc::new(WebState {
        service,
        dev_reload_token: std::env::var("TOPSIDE_DEV_RELOAD_TOKEN").ok(),
    })
}

async fn bind_http_listener(config: &AppConfig) -> Result<(SocketAddr, tokio::net::TcpListener)> {
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .context("invalid server host/port configuration")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed binding http listener at {addr}"))?;

    Ok((addr, listener))
}

fn spawn_http_server(
    listener: tokio::net::TcpListener,
    app: axum::Router,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .context("desktop http server stopped unexpectedly")?;
        Ok(())
    })
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

fn resolve_output_dir(path: Option<PathBuf>) -> Result<PathBuf> {
    let root = match path {
        Some(path) => path,
        None => std::env::current_dir()
            .context("failed reading current working directory")?
            .join("dist"),
    };

    if root.is_absolute() {
        Ok(root)
    } else {
        Ok(std::env::current_dir()?.join(root))
    }
}

fn resolve_absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "topside=info,axum=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .try_init();
}
