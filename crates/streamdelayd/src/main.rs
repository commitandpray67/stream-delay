//! `streamdelayd`: run stream-delay headless, or control a running instance.

mod client;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use streamdelay_config::{Config, MemorySecrets, SecretStore, Secrets};
use streamdelay_control::{
    App, AppError, AppOptions, Overrides, Scope, diagnostics, dump_summary, reachable, scoped_token,
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
    /// dashboard links. Give it the token and ingest key `run` was given, if not
    /// the ones in the settings file (the environment variables are read too).
    Urls {
        #[command(flatten)]
        token: TokenArgs,
        /// The stream key encoders must use.
        #[arg(long, env = "STREAMDELAY_INGEST_KEY", hide_env_values = true)]
        ingest_key: Option<String>,
    },
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
    /// End the broadcast on a running instance: now, without airing what is still
    /// in the delay buffer, or with --after-air once it has aired. Resume with
    /// `streamdelayd resume` or by restarting the stream in OBS.
    End {
        /// Air what stream-delay has received so far, then end.
        #[arg(long)]
        after_air: bool,
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Throw away what has not aired yet and keep broadcasting with the same
    /// delay: viewers see the last stretch again, the overlay slate (if an
    /// overlay is connected), or the last frame, still. Says which.
    Dump {
        /// Don't replay: cover it with the overlay slate (if an overlay is
        /// connected), else hold the last frame.
        #[arg(long)]
        mask: bool,
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
    /// Check that an instance is running and answering: exits with 0 if so, else
    /// 1 (for container health checks). Needs no token.
    Health {
        /// Base URL of the running instance (default: from the config file).
        #[arg(long)]
        url: Option<String>,
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
    #[command(flatten)]
    token: TokenArgs,
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
    #[command(flatten)]
    token: TokenArgs,
}

/// The API token, when not the one in the settings file.
#[derive(Args, Clone, Default)]
struct TokenArgs {
    /// API token (default: from the settings file). Not recommended: other users
    /// of this computer can see command lines. Use --token-file or
    /// STREAMDELAY_TOKEN instead.
    #[arg(long, env = "STREAMDELAY_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// File holding the API token (a Docker or systemd secret, for example).
    /// Wins over --token and STREAMDELAY_TOKEN.
    #[arg(long, env = "STREAMDELAY_TOKEN_FILE", value_name = "PATH")]
    token_file: Option<PathBuf>,
}

fn config_path(cli: &Option<PathBuf>) -> Result<PathBuf> {
    match cli {
        Some(p) => Ok(p.clone()),
        None => Ok(Config::default_path()?),
    }
}

/// The settings file, if there is one and it can be read.
fn read_config(config: &Option<PathBuf>) -> Option<Config> {
    config_path(config)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<Config>(&t).ok())
}

/// Where the running instance is: `url` if given, else the API address in the
/// settings file.
fn instance_url(url: Option<String>, file: Option<&Config>) -> String {
    url.unwrap_or_else(|| {
        let bind = file.map_or(Config::default().api.bind, |c| c.api.bind);
        format!("http://{}", reachable(bind))
    })
}

/// The token in `path`: its content, without surrounding whitespace (such as
/// the line break at the end).
fn read_token_file(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the token file {}", path.display()))?;
    let token = text.trim();
    if token.is_empty() {
        bail!("the token file {} is empty", path.display());
    }
    Ok(token.to_string())
}

impl TokenArgs {
    /// The token given with --token-file (or STREAMDELAY_TOKEN_FILE), else with
    /// --token or STREAMDELAY_TOKEN. An empty one is none: the settings file's
    /// counts.
    fn given(&self) -> Result<Option<String>> {
        match self
            .token_file
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty())
        {
            Some(path) => read_token_file(path).map(Some),
            None => Ok(self.token.clone().filter(|t| !t.is_empty())),
        }
    }
}

impl ApiArgs {
    /// Fills in the URL and token from the config file when not given.
    fn resolve(self, config: &Option<PathBuf>) -> Result<(String, String)> {
        let file = read_config(config);
        let url = instance_url(self.url, file.as_ref());
        let token = self
            .token
            .given()?
            .or_else(|| file.map(|c| c.api.token))
            .filter(|t| !t.is_empty())
            .context(
                "no API token: pass --token-file or set STREAMDELAY_TOKEN, or run \
                 `streamdelayd run` once to create a config",
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
        Cmd::Urls { token, ingest_key } => {
            let path = config_path(&cli.config)?;
            let c = Config::load_or_create(&path)?;
            // What `run` uses: what it was given over what is saved.
            let admin = token.given()?.unwrap_or_else(|| c.api.token.clone());
            for line in url_lines(&c, &admin, ingest_key) {
                println!("{line}");
            }
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
        Cmd::End { after_air, api } => {
            let (url, token) = api.resolve(&cli.config)?;
            let when = if after_air { "after-air" } else { "now" };
            let state = client::post(
                &url,
                &token,
                "/api/v1/stream/end",
                serde_json::json!({ "when": when }),
            )?;
            if state["ending"] == true {
                println!("The stream ends once what is buffered has aired.");
            } else {
                println!("Stream ended. Nothing buffered will air.");
            }
            Ok(())
        }
        Cmd::Dump { mask, api } => {
            let (url, token) = api.resolve(&cli.config)?;
            let mut body = serde_json::json!({});
            if mask {
                body["mode"] = "mask".into();
            }
            let ack = client::post(&url, &token, "/api/v1/stream/dump", body)?;
            println!("{}", dump_summary(&serde_json::from_value(ack)?));
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
        Cmd::Health { url } => {
            let url = instance_url(url, read_config(&cli.config).as_ref());
            let version = client::health(&url)?;
            println!("stream-delay {version} is running at {url}");
            Ok(())
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

/// What `streamdelayd urls` prints for the settings `c`, with the API token
/// `admin` and the ingest key `ingest_key` if given. The addresses are the ones
/// `run` prints: where local clients reach what it listens on.
fn url_lines(c: &Config, admin: &str, ingest_key: Option<String>) -> Vec<String> {
    let host = reachable(c.api.bind);
    let token = |s| scoped_token(admin, s);
    let ingest_key = ingest_key
        .filter(|k| !k.is_empty())
        .or_else(|| c.ingest.key.clone());
    vec![
        format!("OBS server:  rtmp://{}/live", reachable(c.ingest.bind)),
        match ingest_key.as_deref().filter(|k| !k.is_empty()) {
            Some(k) => format!("OBS key:     {k}"),
            None => "OBS key:     any".into(),
        },
        format!("Dashboard:   http://{host}/?token={}", token(Scope::Admin)),
        format!(
            "OBS dock:    http://{host}/dock?token={}",
            token(Scope::Control)
        ),
        format!(
            "Overlay:     http://{host}/overlay?token={}",
            token(Scope::Read)
        ),
    ]
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
        token: args.token.given()?,
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
        // An ephemeral run has no settings file for `streamdelayd urls` to read.
        let show = shown_to_a_person() || config_path.is_none();
        // Without the token, the link only says where the page is.
        let link = |url: &str| {
            if show {
                url.to_string()
            } else {
                url.split_once('?')
                    .map_or(url, |(page, _)| page)
                    .to_string()
            }
        };
        if app.config().ingest.key.is_some_and(|k| !k.is_empty()) {
            println!(
                "OBS server:  {}  (Settings → Stream → Custom)",
                urls.obs_server
            );
            if show {
                println!("OBS key:     {}", urls.obs_key);
            } else {
                println!("OBS key:     (see below)");
            }
        } else {
            println!(
                "OBS server:  {}  (Settings → Stream → Custom, any stream key)",
                urls.obs_server
            );
        }
        println!("Dashboard:   {}", link(&urls.dashboard));
        println!("OBS dock:    {}", link(&urls.dock));
        println!("Overlay:     {}", link(&urls.overlay));
        if !show {
            println!(
                "The links' access tokens and the OBS key are not written to logs: \
                 `streamdelayd urls` shows them (under Docker: \
                 `docker exec <container> streamdelayd urls`)."
            );
        }
        if let Some(p) = &config_path {
            info!("settings file: {}", p.display());
        }
        stop_requested().await?;
        info!("shutting down");
        app.shutdown().await;
        Ok(())
    })
}

/// True when what is printed goes to a person at a terminal. Otherwise (a
/// service manager such as systemd, a container, output sent to a file) it is
/// kept in logs, where the links' tokens and the OBS key would give anyone who
/// can read them control of the stream.
fn shown_to_a_person() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
        && std::env::var_os("container").is_none()
        && !std::path::Path::new("/.dockerenv").exists()
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Cli {
        let argv = std::iter::once("streamdelayd").chain(args.iter().copied());
        match Cli::try_parse_from(argv) {
            Ok(cli) => cli,
            Err(e) => panic!("`{}` refused: {e}", args.join(" ")),
        }
    }

    fn refused(args: &[&str]) -> ErrorKind {
        let args = std::iter::once("streamdelayd").chain(args.iter().copied());
        match Cli::try_parse_from(args) {
            Ok(_) => panic!("accepted"),
            Err(e) => e.kind(),
        }
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn delay_takes_seconds_and_a_mode() {
        let cli = parse(&[
            "delay",
            "12.5",
            "--mask",
            "--url",
            "http://h:1",
            "--token",
            "t",
        ]);
        let Cmd::Delay { seconds, mask, api } = cli.command else {
            panic!("not delay");
        };
        assert_eq!(seconds, 12.5);
        assert!(mask);
        assert_eq!(api.url.as_deref(), Some("http://h:1"));
        assert_eq!(api.token.token.as_deref(), Some("t"));
        let Cmd::Delay { seconds, mask, .. } = parse(&["delay", "0"]).command else {
            panic!("not delay");
        };
        assert_eq!(seconds, 0.0);
        assert!(!mask);
        assert_eq!(refused(&["delay"]), ErrorKind::MissingRequiredArgument);
        assert_eq!(refused(&["delay", "soon"]), ErrorKind::ValueValidation);
    }

    #[test]
    fn stream_commands_take_their_flags() {
        assert!(matches!(
            parse(&["live", "--after-air"]).command,
            Cmd::Live {
                after_air: true,
                ..
            }
        ));
        assert!(matches!(
            parse(&["live"]).command,
            Cmd::Live {
                after_air: false,
                ..
            }
        ));
        assert!(matches!(
            parse(&["end", "--after-air"]).command,
            Cmd::End {
                after_air: true,
                ..
            }
        ));
        assert!(matches!(
            parse(&["dump", "--mask"]).command,
            Cmd::Dump { mask: true, .. }
        ));
        assert!(matches!(
            parse(&["dump"]).command,
            Cmd::Dump { mask: false, .. }
        ));
        assert!(matches!(parse(&["resume"]).command, Cmd::Resume { .. }));
        assert!(matches!(parse(&["state"]).command, Cmd::State { .. }));
        assert!(matches!(parse(&["urls"]).command, Cmd::Urls { .. }));
        let Cmd::Diagnostics { output, .. } = parse(&["diagnostics", "-o", "d.json"]).command
        else {
            panic!("not diagnostics");
        };
        assert_eq!(output, Some(PathBuf::from("d.json")));
        // Dump has no replay flag: replaying is what it does without --mask.
        assert_eq!(refused(&["dump", "--replay"]), ErrorKind::UnknownArgument);
        assert_eq!(refused(&["pause"]), ErrorKind::InvalidSubcommand);
        assert_eq!(
            refused(&[]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn the_config_file_may_follow_the_command() {
        let cli = parse(&["state", "--config", "/etc/sd.toml"]);
        assert_eq!(cli.config, Some(PathBuf::from("/etc/sd.toml")));
        let cli = parse(&["--config", "/etc/sd.toml", "run", "--ephemeral"]);
        assert_eq!(cli.config, Some(PathBuf::from("/etc/sd.toml")));
    }

    #[test]
    fn run_takes_addresses_and_settings() {
        let Cmd::Run(a) = parse(&[
            "run",
            "--ingest",
            "0.0.0.0:1935",
            "--api",
            "[::1]:7788",
            "--dest",
            "rtmp://live.twitch.tv/app",
            "--delay",
            "30",
            "--max-delay",
            "600",
            "--grace",
            "20",
            "--allow-lan",
            "--no-keychain",
        ])
        .command
        else {
            panic!("not run");
        };
        assert_eq!(a.ingest, Some("0.0.0.0:1935".parse().unwrap()));
        assert_eq!(a.api, Some("[::1]:7788".parse().unwrap()));
        assert_eq!(a.dest.as_deref(), Some("rtmp://live.twitch.tv/app"));
        assert_eq!(a.delay, Some(30.0));
        assert_eq!(a.max_delay, Some(600));
        assert_eq!(a.grace, Some(20));
        assert!(a.allow_lan && a.no_keychain);
        assert!(!a.ephemeral && !a.passthrough);
        assert_eq!(a.key_env, "STREAMDELAY_KEY");
        // A name, not an address: resolving it would pick one of its addresses
        // silently.
        assert_eq!(
            refused(&["run", "--ingest", "localhost:1935"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            refused(&["run", "--max-delay", "2.5"]),
            ErrorKind::ValueValidation
        );
    }

    fn write_config(dir: &std::path::Path, bind: &str, token: &str) -> Option<PathBuf> {
        let mut c = Config::default();
        c.api.bind = bind.parse().unwrap();
        c.api.token = token.into();
        let path = dir.join("config.toml");
        c.save(&path).unwrap();
        Some(path)
    }

    fn api(url: Option<&str>, token: Option<&str>) -> ApiArgs {
        ApiArgs {
            url: url.map(str::to_string),
            token: TokenArgs {
                token: token.map(str::to_string),
                token_file: None,
            },
        }
    }

    #[test]
    fn the_client_finds_the_instance_in_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "0.0.0.0:7790", "from-the-file-0123");
        // An instance listening on every interface is reached over loopback.
        let (url, token) = api(None, None).resolve(&path).unwrap();
        assert_eq!(url, "http://127.0.0.1:7790");
        assert_eq!(token, "from-the-file-0123");
        // What is given wins over the file.
        let (url, token) = api(Some("http://nas:1"), Some("given"))
            .resolve(&path)
            .unwrap();
        assert_eq!((url.as_str(), token.as_str()), ("http://nas:1", "given"));
    }

    #[test]
    fn the_client_needs_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Some(dir.path().join("none.toml"));
        let err = api(None, None).resolve(&missing).unwrap_err();
        assert!(err.to_string().contains("no API token"), "{err}");
        // Defaults to the default address when there is no file.
        let (url, _) = api(None, Some("t")).resolve(&missing).unwrap();
        assert_eq!(
            url,
            format!("http://{}", reachable(Config::default().api.bind))
        );
        // An empty token is no token, whether given or in the file.
        assert!(api(None, Some("")).resolve(&missing).is_err());
        let path = write_config(dir.path(), "127.0.0.1:7790", "");
        assert!(api(None, None).resolve(&path).is_err());
    }

    #[test]
    fn health_and_token_files_parse() {
        let Cmd::Health { url } = parse(&["health", "--url", "http://127.0.0.1:7788"]).command
        else {
            panic!("not health");
        };
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:7788"));
        assert!(matches!(
            parse(&["health"]).command,
            Cmd::Health { url: None }
        ));
        // Health needs no token, so takes none.
        assert_eq!(
            refused(&["health", "--token", "t"]),
            ErrorKind::UnknownArgument
        );
        let Cmd::Run(a) = parse(&["run", "--token-file", "/run/secrets/token"]).command else {
            panic!("not run");
        };
        assert_eq!(
            a.token.token_file,
            Some(PathBuf::from("/run/secrets/token"))
        );
        let Cmd::State { api } = parse(&["state", "--token-file", "t.txt"]).command else {
            panic!("not state");
        };
        assert_eq!(api.token.token_file, Some(PathBuf::from("t.txt")));
    }

    #[test]
    fn a_token_file_wins_and_must_hold_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "  from-the-file-0123\r\n").unwrap();
        let given = |token: Option<&str>, file: Option<&Path>| {
            TokenArgs {
                token: token.map(str::to_string),
                token_file: file.map(Path::to_path_buf),
            }
            .given()
        };
        let token = given(Some("given"), Some(&file)).unwrap();
        assert_eq!(token.as_deref(), Some("from-the-file-0123"));
        assert_eq!(
            given(Some("given"), None).unwrap().as_deref(),
            Some("given")
        );
        // The client too, over the settings file's.
        let config = write_config(dir.path(), "127.0.0.1:7790", "in-the-settings");
        let mut args = api(None, None);
        args.token.token_file = Some(file.clone());
        assert_eq!(args.resolve(&config).unwrap().1, "from-the-file-0123");

        std::fs::write(&file, "\n").unwrap();
        let err = given(None, Some(&file)).unwrap_err();
        assert!(err.to_string().contains("is empty"), "{err}");
        let missing = dir.path().join("missing");
        let err = given(None, Some(&missing)).unwrap_err();
        assert!(err.to_string().contains("reading the token file"), "{err}");
    }

    #[test]
    fn an_empty_token_is_none() {
        // STREAMDELAY_TOKEN= (set, but empty) or STREAMDELAY_TOKEN_FILE=.
        let empty = TokenArgs {
            token: Some(String::new()),
            token_file: Some(PathBuf::new()),
        };
        assert_eq!(empty.given().unwrap(), None);
        // The settings file's counts then.
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), "127.0.0.1:7790", "in-the-settings");
        let args = ApiArgs {
            url: None,
            token: empty,
        };
        assert_eq!(args.resolve(&config).unwrap().1, "in-the-settings");
    }

    #[test]
    fn printed_links_reach_the_addresses_listened_on() {
        let links = |api: &str, ingest: &str| {
            let mut c = Config::default();
            c.api.bind = api.parse().unwrap();
            c.ingest.bind = ingest.parse().unwrap();
            url_lines(&c, "0123456789abcdef", None).join("\n")
        };
        // Every interface: over loopback, as `run` prints them.
        let all = links("0.0.0.0:7790", "0.0.0.0:1940");
        assert!(all.contains("rtmp://127.0.0.1:1940/live"), "{all}");
        assert!(all.contains("http://127.0.0.1:7790/?token="), "{all}");
        // IPv6 only, and one address of this computer: 127.0.0.1 would not
        // connect.
        let v6 = links("[::1]:7790", "[::]:1940");
        assert!(v6.contains("rtmp://[::1]:1940/live"), "{v6}");
        assert!(v6.contains("http://[::1]:7790/?token="), "{v6}");
        assert!(v6.contains("http://[::1]:7790/dock?token="), "{v6}");
        assert!(v6.contains("http://[::1]:7790/overlay?token="), "{v6}");
        let lan = links("192.168.1.5:7790", "192.168.1.5:1940");
        assert!(lan.contains("rtmp://192.168.1.5:1940/live"), "{lan}");
        assert!(lan.contains("http://192.168.1.5:7790/?token="), "{lan}");
    }

    #[test]
    fn printed_links_carry_the_tokens_and_key_run_uses() {
        let mut c = Config::default();
        c.ingest.key = Some("saved-ingest-key".into());
        let admin = "0123456789abcdef";
        let text = url_lines(&c, admin, None).join("\n");
        assert!(text.contains("OBS key:     saved-ingest-key"), "{text}");
        for s in [Scope::Admin, Scope::Control, Scope::Read] {
            assert!(text.contains(&scoped_token(admin, s)), "{text}");
        }
        // What `run` is given wins; an empty one is none.
        let given = url_lines(&c, admin, Some("given-ingest-key".into())).join("\n");
        assert!(given.contains("OBS key:     given-ingest-key"), "{given}");
        let empty = url_lines(&c, admin, Some(String::new())).join("\n");
        assert!(empty.contains("OBS key:     saved-ingest-key"), "{empty}");
        c.ingest.key = None;
        let any = url_lines(&c, admin, None).join("\n");
        assert!(any.contains("OBS key:     any"), "{any}");
    }

    #[test]
    fn a_port_in_use_says_what_to_do() {
        let busy = AppError::Bind {
            addr: "127.0.0.1:7788".parse().unwrap(),
            source: std::io::ErrorKind::AddrInUse.into(),
        };
        let text = format!("{:#}", startup_error(busy));
        assert!(text.contains("already in use"), "{text}");
        assert!(text.contains("--api and --ingest"), "{text}");
        let other = AppError::Bind {
            addr: "127.0.0.1:7788".parse().unwrap(),
            source: std::io::ErrorKind::PermissionDenied.into(),
        };
        let text = format!("{:#}", startup_error(other));
        assert!(!text.contains("already in use"), "{text}");
        assert!(text.contains("starting stream-delay"), "{text}");
    }
}
