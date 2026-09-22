//! `streamdelayd`: run the relay headless, or control a running instance.

mod client;

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use streamdelay_control::{ControlConfig, Preset};
use streamdelay_relay::{DelayMode, Destination, DestinationKey, EngineConfig, RelayConfig};
use tracing::info;

#[derive(Parser)]
#[command(
    name = "streamdelayd",
    version,
    about = "Change your stream delay while live."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the relay (OBS -> stream-delay -> destination).
    Run(RunArgs),
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
    /// Print the state of a running instance as JSON.
    State {
        #[command(flatten)]
        api: ApiArgs,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Address OBS streams to (use rtmp://<this>/live in OBS).
    #[arg(long, default_value = "127.0.0.1:1935")]
    ingest: SocketAddr,
    /// Destination URL, for example rtmp://live.twitch.tv/app.
    #[arg(long)]
    dest: Option<String>,
    /// Environment variable holding the destination stream key.
    #[arg(long, default_value = "STREAMDELAY_KEY")]
    key_env: String,
    /// Forward the stream key OBS uses instead of a configured one.
    #[arg(long)]
    passthrough: bool,
    /// Address of the control API and web UI.
    #[arg(long, default_value = "127.0.0.1:7788")]
    api: SocketAddr,
    /// API token. Generated if not set.
    #[arg(long, env = "STREAMDELAY_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Maximum delay in seconds.
    #[arg(long, default_value_t = 120)]
    max_delay: u64,
    /// Delay to start with, in seconds.
    #[arg(long, default_value_t = 0.0)]
    delay: f64,
    /// Seconds to keep the destination connected after OBS disconnects.
    #[arg(long, default_value_t = 30)]
    grace: u64,
}

#[derive(Args, Clone)]
struct ApiArgs {
    /// Base URL of the running instance.
    #[arg(long, default_value = "http://127.0.0.1:7788")]
    url: String,
    /// API token printed by `streamdelayd run`.
    #[arg(long, env = "STREAMDELAY_TOKEN", hide_env_values = true)]
    token: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Run(args) => run(args),
        Cmd::Delay { seconds, mask, api } => {
            let mode = if mask { "mask" } else { "rewind" };
            client::print(client::put(
                &api.url,
                &api.token,
                "/api/v1/delay",
                serde_json::json!({
                    "seconds": seconds,
                    "mode": mode,
                }),
            )?)
        }
        Cmd::Live { after_air, api } => {
            let when = if after_air { "after-air" } else { "now" };
            client::print(client::post(
                &api.url,
                &api.token,
                "/api/v1/live",
                serde_json::json!({
                    "when": when,
                }),
            )?)
        }
        Cmd::State { api } => client::print(client::get(&api.url, &api.token, "/api/v1/state")?),
    }
}

fn run(args: RunArgs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,streamdelay=info".into()),
        )
        .init();
    let destination = match args.dest {
        None => None,
        Some(url) => {
            let key = if args.passthrough {
                DestinationKey::Passthrough
            } else {
                match std::env::var(&args.key_env) {
                    Ok(k) if !k.is_empty() => DestinationKey::Fixed(k),
                    // The key may also be embedded in the URL.
                    _ => DestinationKey::Fixed(String::new()),
                }
            };
            Some(Destination { url, key })
        }
    };
    let token = args
        .token
        .unwrap_or_else(streamdelay_control::generate_token);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let relay = streamdelay_relay::start(RelayConfig {
            ingest_bind: args.ingest,
            destination,
            engine: EngineConfig {
                max_delay_ms: args.max_delay * 1000,
                ..Default::default()
            },
            encoder_grace: Duration::from_secs(args.grace),
            ..Default::default()
        })
        .await
        .context("starting the relay")?;
        if args.delay > 0.0 {
            relay
                .set_delay((args.delay * 1000.0) as u64, DelayMode::Rewind)
                .await?;
        }
        let server = streamdelay_control::serve(
            relay.clone(),
            ControlConfig {
                bind: args.api,
                token: token.clone(),
                allow_lan: false,
                presets: Preset::defaults(),
            },
        )
        .await
        .context("starting the control API")?;
        info!(
            "OBS: set Server to rtmp://{}/live (any stream key)",
            relay.ingest_addr()
        );
        info!(
            "Control API: http://{} (token in STREAMDELAY_TOKEN or --token)",
            server.addr
        );
        if std::env::var("STREAMDELAY_TOKEN").is_err() {
            println!("API token: {token}");
        }
        tokio::signal::ctrl_c().await?;
        info!("shutting down");
        relay.shutdown().await;
        Ok(())
    })
}
