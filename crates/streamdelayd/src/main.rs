//! `streamdelayd`: run stream-delay headless, or control a running instance.

mod client;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use streamdelay_config::{Config, MemorySecrets, SecretStore, Secrets};
use streamdelay_control::{
    App, AppError, AppOptions, Overrides, Scope, diagnostics, reachable, scoped_token,
};
use streamdelay_relay::RelayError;
use tracing::info;
use tracing_subscriber::prelude::*;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[derive(Parser)]
#[command(
    name = "streamdelayd",
    version,
    about = "Change your stream delay while live."
)]
struct Cli {
    /// Config file (default: the platform config directory).
    #[arg(long, global = true, env = "STREAMDELAY_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the relay (OBS -> stream-delay -> destination) with the web UI and API.
    Run(RunArgs),
    /// Print the OBS server address and stream key, and the dock, overlay and
    /// dashboard links.
    Urls,
    /// Set the delay on a running instance.
    Delay {
        /// Delay in seconds. 0 goes live.
        seconds: f64,
        /// Cover the change with the overlay slate instead of rewinding.
        #[arg(long)]
        mask: bool,
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Drop the delay on a running instance.
    Live {
        /// Air everything already buffered first, then go live.
        #[arg(long)]
        after_air: bool,
        #[command(flatten)]
        api: ApiArgs,
    },
    /// End the broadcast now on a running instance. Nothing still in the delay
    /// buffer airs. Resume with `streamdelayd resume` or by restarting the stream
    /// in OBS.
    End {
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Broadcast again after `streamdelayd end`.
    Resume {
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Print the state of a running instance as JSON.
    State {
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Save a diagnostics file (settings, state, recent logs; secrets removed) from a
    /// running instance, to attach to bug reports.
    Diagnostics {
        /// Where to write it (default: print to the terminal).
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        api: ApiArgs,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Address OBS streams to (overrides the config file).
    #[arg(long)]
    ingest: Option<SocketAddr>,
    /// Destination URL, for example rtmp://live.twitch.tv/app.
    #[arg(long)]
    dest: Option<String>,
    /// Environment variable holding the destination stream key for this run.
    #[arg(long, default_value = "STREAMDELAY_KEY")]
    key_env: String,
    /// Forward the stream key OBS uses instead of a stored one.
    #[arg(long)]
    passthrough: bool,
    /// Address of the control API and web UI.
    #[arg(long)]
    api: Option<SocketAddr>,
    /// API token (overrides the one in the config file).
    #[arg(long, env = "STREAMDELAY_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Maximum delay in seconds.
    #[arg(long)]
    max_delay: Option<u64>,
    /// Delay to start with, in seconds.
    #[arg(long)]
    delay: Option<f64>,
    /// Seconds to keep the destination connected after OBS disconnects.
    #[arg(long)]
    grace: Option<u64>,
    /// Do not read or write a config file; keep everything in memory.
    #[arg(long)]
    ephemeral: bool,
    /// Store secrets in a private file instead of the OS keychain.
    #[arg(long)]
    no_keychain: bool,
    /// Accept API requests addressed to non-loopback hosts (still token-protected).
    #[arg(long)]
    allow_lan: bool,
    /// Require encoders to publish with this stream key.
    #[arg(long, env = "STREAMDELAY_INGEST_KEY", hide_env_values = true)]
    ingest_key: Option<String>,
}

#[derive(Args, Clone)]
struct ApiArgs {
    /// Base URL of the running instance (default: from the config file).
    #[arg(long)]
    url: Option<String>,
    /// API token (default: from the config file).
    #[arg(long, env = "STREAMDELAY_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

fn config_path(cli: &Option<PathBuf>) -> Result<PathBuf> {
    match cli {
        Some(p) => Ok(p.clone()),
        None => Ok(Config::default_path()?),
    }
}

impl ApiArgs {
    /// Fills in the URL and token from the config file when not given.
    fn resolve(self, config: &Option<PathBuf>) -> Result<(String, String)> {
        let file = config_path(config)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str::<Config>(&t).ok());
        let url = self.url.unwrap_or_else(|| {
            let bind = file
                .as_ref()
                .map_or(Config::default().api.bind, |c| c.api.bind);
            format!("http://{}", reachable(bind))
        });
        let token = self
            .token
            .or_else(|| file.map(|c| c.api.token))
            .filter(|t| !t.is_empty())
            .context(
                "no API token: pass --token or run `streamdelayd run` once to create a config",
            )?;
        Ok((url, token))
    }
}

fn main() -> Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Run(args) => run(cli.config, args),
        Cmd::Urls => {
            let path = config_path(&cli.config)?;
            let c = Config::load_or_create(&path)?;
            let host = format!("127.0.0.1:{}", c.api.bind.port());
            let token = |s| scoped_token(&c.api.token, s);
            println!(
                "OBS server:  rtmp://127.0.0.1:{}/live",
                c.ingest.bind.port()
            );
            match c.ingest.key.as_deref().filter(|k| !k.is_empty()) {
                Some(k) => println!("OBS key:     {k}"),
                None => println!("OBS key:     any"),
            }
            println!("Dashboard:   http://{host}/?token={}", token(Scope::Admin));
            println!(
                "OBS dock:    http://{host}/dock?token={}",
                token(Scope::Control)
            );
            println!(
                "Overlay:     http://{host}/overlay?token={}",
                token(Scope::Read)
            );
            Ok(())
        }
        Cmd::Delay { seconds, mask, api } => {
            let (url, token) = api.resolve(&cli.config)?;
            let mut body = serde_json::json!({ "seconds": seconds });
            if mask {
                body["mode"] = "mask".into();
            }
            client::print(client::put(&url, &token, "/api/v1/delay", body)?)
        }
        Cmd::Live { after_air, api } => {
            let (url, token) = api.resolve(&cli.config)?;
            let when = if after_air { "after-air" } else { "now" };
            client::print(client::post(
                &url,
                &token,
                "/api/v1/live",
                serde_json::json!({ "when": when }),
            )?)
        }
        Cmd::End { api } => {
            let (url, token) = api.resolve(&cli.config)?;
            client::post(&url, &token, "/api/v1/stream/end", serde_json::json!({}))?;
            println!("Stream ended. Nothing buffered will air.");
            Ok(())
        }
        Cmd::Resume { api } => {
            let (url, token) = api.resolve(&cli.config)?;
            client::post(&url, &token, "/api/v1/stream/resume", serde_json::json!({}))?;
            println!("Broadcasting again.");
            Ok(())
        }
        Cmd::State { api } => {
            let (url, token) = api.resolve(&cli.config)?;
            client::print(client::get(&url, &token, "/api/v1/state")?)
        }
        Cmd::Diagnostics { output, api } => {
            let (url, token) = api.resolve(&cli.config)?;
            let bundle = client::get(&url, &token, "/api/v1/diagnostics")?;
            match output {
                Some(path) => {
                    std::fs::write(&path, serde_json::to_string_pretty(&bundle)?)
                        .with_context(|| format!("writing {}", path.display()))?;
                    println!("Saved diagnostics to {}", path.display());
                    Ok(())
                }
                None => client::print(bundle),
            }
        }
    }
}

fn run(config: Option<PathBuf>, args: RunArgs) -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,obws=error".into()),
        )
        // No color codes when logs go to a file or `docker logs`.
        .with(tracing_subscriber::fmt::layer().with_ansi(std::io::stderr().is_terminal()))
        .with(diagnostics::layer())
        .init();
    let config_path = if args.ephemeral {
        None
    } else {
        Some(config_path(&config)?)
    };
    // Ephemeral runs keep secrets in memory: a file under the shared temp directory
    // would be at a predictable path other users could interfere with.
    let secrets: Arc<dyn SecretStore> = match &config_path {
        Some(p) => Arc::new(Secrets::new(
            &p.parent().map(PathBuf::from).unwrap_or_default(),
            !args.no_keychain,
        )),
        None => Arc::new(MemorySecrets::default()),
    };
    let overrides = Overrides {
        ingest: args.ingest,
        api: args.api,
        destination_url: args.dest,
        destination_key: std::env::var(&args.key_env).ok().filter(|k| !k.is_empty()),
        passthrough: args.passthrough,
        token: args.token,
        max_delay_seconds: args.max_delay,
        start_delay_seconds: args.delay,
        grace_seconds: args.grace,
        allow_lan: args.allow_lan,
        ingest_key: args.ingest_key,
    };
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let app = App::start(AppOptions {
            config_path: config_path.clone(),
            secrets,
            overrides,
        })
        .await
        .map_err(startup_error)?;
        let urls = app.urls();
        if app.config().ingest.key.is_some_and(|k| !k.is_empty()) {
            println!(
                "OBS server:  {}  (Settings → Stream → Custom)",
                urls.obs_server
            );
            println!("OBS key:     {}", urls.obs_key);
        } else {
            println!(
                "OBS server:  {}  (Settings → Stream → Custom, any stream key)",
                urls.obs_server
            );
        }
        println!("Dashboard:   {}", urls.dashboard);
        println!("OBS dock:    {}", urls.dock);
        println!("Overlay:     {}", urls.overlay);
        if let Some(p) = &config_path {
            info!("settings file: {}", p.display());
        }
        stop_requested().await?;
        info!("shutting down");
        app.shutdown().await;
        Ok(())
    })
}

/// Waits for Ctrl+C, or for the request to stop that service managers send:
/// SIGTERM from `docker stop` and systemd (ignored otherwise, as the container's
/// first process), or the console window closing on Windows. Stopping then ends
/// the broadcast cleanly instead of being killed mid-stream.
async fn stop_requested() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
        let (mut brk, mut close, mut shutdown) = (ctrl_break()?, ctrl_close()?, ctrl_shutdown()?);
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = brk.recv() => Ok(()),
            _ = close.recv() => Ok(()),
            _ = shutdown.recv() => Ok(()),
        }
    }
    #[cfg(not(any(unix, windows)))]
    tokio::signal::ctrl_c().await
}

/// Adds what to do about the most common startup failure, a port in use.
fn startup_error(e: AppError) -> anyhow::Error {
    let in_use = matches!(
        &e,
        AppError::Bind { source, .. } | AppError::Relay(RelayError::Bind { source, .. })
            if source.kind() == std::io::ErrorKind::AddrInUse
    );
    let err = anyhow::Error::new(e).context("starting stream-delay");
    if in_use {
        err.context(
            "a port stream-delay needs is already in use. Is stream-delay (or its desktop app) \
             already running? Close it, or choose other ports with --api and --ingest.",
        )
    } else {
        err
    }
}
