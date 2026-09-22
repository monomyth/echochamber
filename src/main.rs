use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use echochamber::config::{self, Config};

#[derive(Parser)]
#[command(
    name = "echochamber",
    about = "Send one live stream to YouTube, X, and Twitch"
)]
struct Cli {
    /// Config file (overrides ECHOCHAMBER_CONFIG / ./config.toml / ~/.config/echochamber/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Control HTTP host:port (client commands). Defaults to config control.bind.
    #[arg(long, global = true)]
    control: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a commented config.toml
    Init {
        #[arg(long, default_value = "config.toml")]
        path: PathBuf,
    },
    /// Check config, binaries, and (with --verbose) ffmpeg command templates
    Validate {
        #[arg(long)]
        verbose: bool,
    },
    /// Run the ingest daemon + control plane
    Serve {
        /// Listen on this IP for both RTMP and HTTP (ports from config). Example: 0.0.0.0
        #[arg(long)]
        host: Option<String>,
        /// Override ingest.bind entirely (e.g. 0.0.0.0:1935)
        #[arg(long)]
        ingest_bind: Option<String>,
        /// Override control.bind for this process (e.g. 0.0.0.0:8080)
        #[arg(long)]
        control_bind: Option<String>,
    },
    /// GET /status as JSON
    Status,
    /// POST /reload
    Reload,
    /// POST /stop
    Stop,
    /// POST /push — add a temporary extra RTMP destination
    Push {
        #[arg(long)]
        url: String,
        #[arg(long)]
        name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("echochamber=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { path } => {
            config::write_init_template(&path)?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Cmd::Validate { verbose } => {
            let (cfg, path) = Config::discover(cli.config.as_deref())?;
            println!("config             {}", path.display());
            echochamber::validate::run(&cfg, &path, verbose)
        }
        Cmd::Serve {
            host,
            ingest_bind,
            control_bind,
        } => {
            let (mut cfg, path) = Config::discover(cli.config.as_deref())?;
            cfg.apply_bind_overrides(
                host.as_deref(),
                ingest_bind.as_deref(),
                control_bind.as_deref(),
            )?;
            echochamber::daemon::serve(cfg, path).await
        }
        Cmd::Status => {
            let addr = control_addr(&cli)?;
            let body = echochamber::control::client_get_status(&addr).await?;
            match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
                Err(_) => print!("{body}"),
            }
            Ok(())
        }
        Cmd::Reload => {
            let addr = control_addr(&cli)?;
            print!(
                "{}",
                echochamber::control::client_post(&addr, "/reload", None).await?
            );
            Ok(())
        }
        Cmd::Stop => {
            let addr = control_addr(&cli)?;
            match echochamber::control::client_post(&addr, "/stop", None).await {
                Ok(body) => {
                    print!("{body}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("no daemon on {addr} ({e})");
                    Ok(())
                }
            }
        }
        Cmd::Push { url, name } => {
            let (cfg, _) = Config::discover(cli.config.as_deref())
                .unwrap_or_else(|_| (Config::default(), PathBuf::from("config.toml")));
            let addr = cli
                .control
                .clone()
                .unwrap_or_else(|| cfg.client_control_bind());
            let mut json = serde_json::json!({ "url": url });
            if let Some(n) = name {
                json["name"] = serde_json::Value::String(n);
            }
            print!(
                "{}",
                echochamber::control::client_post(&addr, "/push", Some(json)).await?
            );
            Ok(())
        }
    }
}

fn control_addr(cli: &Cli) -> Result<String> {
    if let Some(c) = &cli.control {
        return Ok(c.clone());
    }
    match Config::discover(cli.config.as_deref()) {
        Ok((cfg, _)) => Ok(cfg.client_control_bind()),
        Err(_) => bail!("cannot find config; pass --control HOST:PORT"),
    }
}
