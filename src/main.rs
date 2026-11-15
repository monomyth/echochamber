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
    /// Write config.toml and the standby video next to it
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
    /// Provision one ephemeral relay and print its ingest URL when it is ready
    Deploy {
        /// hetzner, aws-lightsail, or local
        #[arg(long)]
        provider: String,
        /// Hetzner location (fsn1) or Lightsail region (us-east-1)
        #[arg(long, default_value = "")]
        region: String,
        #[arg(long)]
        zone: Option<String>,
        #[arg(long)]
        image: Option<String>,
        #[arg(long)]
        size: Option<String>,
        /// Image already contains echochamber and ffmpeg
        #[arg(long)]
        prebuilt: bool,
        /// https URL of an echochamber binary, used when the image does not have one
        #[arg(long)]
        binary_url: Option<String>,
        #[arg(long, default_value_t = echochamber::deploy::DEFAULT_TTL_SECS)]
        ttl_secs: u64,
        #[arg(long, default_value_t = echochamber::deploy::DEFAULT_IDLE_SECS)]
        idle_secs: u64,
        #[arg(long, default_value_t = echochamber::deploy::DEFAULT_GRACE_SECS)]
        connect_grace_secs: u64,
        #[arg(long, default_value = "0.0.0.0/0")]
        ingest_cidr: String,
        #[arg(long, default_value_t = echochamber::deploy::DEFAULT_READY_TIMEOUT)]
        ready_timeout_secs: u64,
    },
    /// List relays this machine has provisioned
    Deployments {
        #[arg(long)]
        id: Option<String>,
    },
    /// Destroy one relay
    Teardown {
        #[arg(long)]
        id: String,
    },
    /// Delete labeled relays past their deadline, or already stopped
    Reap {
        #[arg(long)]
        provider: String,
        #[arg(long, default_value = "")]
        region: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Print cloud-init for the Hetzner or Lightsail console. Contains a stream key.
    CloudInit {
        #[arg(long, default_value = "hetzner")]
        provider: String,
        #[arg(long, default_value_t = echochamber::deploy::DEFAULT_TTL_SECS)]
        ttl_secs: u64,
        #[arg(long, default_value_t = echochamber::deploy::DEFAULT_IDLE_SECS)]
        idle_secs: u64,
        #[arg(long, default_value_t = echochamber::deploy::DEFAULT_GRACE_SECS)]
        connect_grace_secs: u64,
        #[arg(long)]
        prebuilt: bool,
        #[arg(long)]
        binary_url: Option<String>,
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
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let token = control_token(&cli);
    let seed = relay_seed(&cli);
    match cli.cmd {
        Cmd::Init { path } => {
            let report = config::init_at(&path)?;
            if report.config_written {
                println!("wrote {}", path.display());
            } else {
                println!("{} already exists — left it in place", path.display());
            }
            for asset in report.assets {
                if asset.wrote {
                    println!("wrote {}", asset.path.display());
                } else {
                    println!("{} already exists — left it in place", asset.path.display());
                }
            }
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
            let body = echochamber::control::client_get_status(&addr, token.as_deref()).await?;
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
                echochamber::control::client_post(&addr, "/reload", None, token.as_deref()).await?
            );
            Ok(())
        }
        Cmd::Stop => {
            let addr = control_addr(&cli)?;
            match echochamber::control::client_post(&addr, "/stop", None, token.as_deref()).await {
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
                echochamber::control::client_post(&addr, "/push", Some(json), token.as_deref(),)
                    .await?
            );
            Ok(())
        }
        Cmd::Deploy {
            provider,
            region,
            zone,
            image,
            size,
            prebuilt,
            binary_url,
            ttl_secs,
            idle_secs,
            connect_grace_secs,
            ingest_cidr,
            ready_timeout_secs,
        } => {
            let request = echochamber::deploy::DeployRequest {
                provider: provider.parse()?,
                region,
                zone,
                image,
                size,
                prebuilt,
                binary_url,
                ttl_secs,
                idle_secs,
                connect_grace_secs,
                ingest_cidr,
                ready_timeout_secs,
            };
            let record = echochamber::deploy::DeployService::from_env()
                .deploy(request, seed)
                .await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        Cmd::Deployments { id } => {
            let service = echochamber::deploy::DeployService::from_env();
            if let Some(id) = id {
                println!("{}", serde_json::to_string_pretty(&service.get(&id)?)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&service.list()?)?);
            }
            Ok(())
        }
        Cmd::Teardown { id } => {
            let record = echochamber::deploy::DeployService::from_env()
                .teardown(&id)
                .await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        Cmd::Reap {
            provider,
            region,
            dry_run,
        } => {
            let report = echochamber::deploy::DeployService::from_env()
                .reap(echochamber::deploy::ReapRequest {
                    provider: provider.parse()?,
                    region,
                    dry_run,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.failed.is_empty() {
                bail!("{} reap failure(s)", report.failed.len());
            }
            Ok(())
        }
        Cmd::CloudInit {
            provider,
            ttl_secs,
            idle_secs,
            connect_grace_secs,
            prebuilt,
            binary_url,
        } => {
            let kind: echochamber::deploy::ProviderKind = provider.parse()?;
            match kind {
                echochamber::deploy::ProviderKind::Hetzner
                | echochamber::deploy::ProviderKind::AwsLightsail => {}
                echochamber::deploy::ProviderKind::Local => {
                    bail!("cloud-init is for hetzner and aws-lightsail");
                }
            }
            let stream_key = format!("{:016x}{:016x}", rand_u64(), rand_u64());
            let token = format!(
                "{:016x}{:016x}{:016x}{:016x}",
                rand_u64(),
                rand_u64(),
                rand_u64(),
                rand_u64()
            );
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let script = echochamber::deploy::cloud_init_script(
                &stream_key,
                &token,
                now + ttl_secs,
                idle_secs,
                connect_grace_secs,
                prebuilt,
                binary_url.as_deref(),
                &seed.platforms,
                &seed.quality,
            )?;
            eprintln!("Paste this into the server user-data field. It contains a stream key. Do not commit it.");
            eprintln!(
                "Create a firewall that allows TCP 1935 only, from the ingest CIDR you chose."
            );
            eprintln!("OBS server: rtmp://<server-ip>:1935/echochamber");
            eprintln!("OBS stream key: {stream_key}");
            println!("{script}");
            Ok(())
        }
    }
}

fn rand_u64() -> u64 {
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).expect("random");
    u64::from_le_bytes(buf)
}

fn relay_seed(cli: &Cli) -> echochamber::deploy::RelaySeed {
    match Config::discover(cli.config.as_deref()) {
        Ok((cfg, _)) => echochamber::deploy::RelaySeed {
            platforms: cfg.platforms,
            quality: cfg.quality,
        },
        Err(_) => echochamber::deploy::RelaySeed::default(),
    }
}

fn control_token(cli: &Cli) -> Option<String> {
    Config::discover(cli.config.as_deref())
        .ok()
        .map(|(cfg, _)| cfg.control.token)
        .filter(|token| !token.is_empty())
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
