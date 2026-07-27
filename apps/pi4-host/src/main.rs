//! Vinyl Bridge — Raspberry Pi 4 Linux host.
//!
//! Same control surface as `apps/p4-bridge` (HTTP API, mDNS, capture gate,
//! record, session manager). AirPlay 2 / Chromecast protocol stacks are
//! scaffolded as stubs until ported from `apps/firmware/components/airplay2`.

mod airplay;
mod capture;
mod chromecast;
mod config;
mod control;
mod mdns_svc;
mod record;
mod session;
mod stats;
mod webapi;

use crate::airplay::AirplayManager;
use crate::chromecast::ChromecastManager;
use crate::config::{Config, TargetConfig};
use crate::control::{ControlState, SpeakerTarget, TargetType};
use crate::mdns_svc::MdnsHub;
use crate::record::Recorder;
use crate::session::SessionRuntime;
use crate::stats::Stats;
use crate::webapi::AppState;
use anyhow::{Context, Result};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "vinyl-bridge-pi", version, about = "Vinyl Audio Bridge host for Raspberry Pi 4")]
struct Cli {
    /// Path to TOML config (default: /etc/vinyl-bridge/config.toml if present).
    #[arg(long, env = "VINYL_CONFIG")]
    config: Option<PathBuf>,

    /// Override HTTP bind address (e.g. 0.0.0.0:8080 for non-root bring-up).
    #[arg(long, env = "VINYL_HTTP_BIND")]
    http_bind: Option<String>,

    /// Log filter (RUST_LOG style).
    #[arg(long, default_value = "info", env = "RUST_LOG")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config_path = cli.config.or_else(|| {
        let p = PathBuf::from("/etc/vinyl-bridge/config.toml");
        if p.exists() {
            Some(p)
        } else {
            None
        }
    });

    let mut cfg = Config::load(config_path.as_deref())?;
    if let Some(bind) = cli.http_bind {
        cfg.http_bind = bind;
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        device = %cfg.device_name,
        bind = %cfg.http_bind,
        capture = %cfg.capture_device,
        "vinyl-bridge-pi starting"
    );

    let initial_targets: Vec<SpeakerTarget> = cfg
        .targets
        .iter()
        .map(target_from_cfg)
        .collect();
    let control = ControlState::new(cfg.volume_db, initial_targets);
    let capture = capture::start(&cfg);
    let recorder = Recorder::new(&cfg);
    let stats = Arc::new(Stats::new());
    let airplay = Arc::new(AirplayManager::new());
    let chromecast = Arc::new(ChromecastManager::new());

    let http_port = parse_port(&cfg.http_bind).unwrap_or(80);
    let mdns = MdnsHub::start(&cfg.mdns_hostname, &cfg.device_name, http_port)
        .context("mDNS start")?;

    let state = AppState {
        cfg: cfg.clone(),
        control: control.clone(),
        capture: capture.clone(),
        recorder: recorder.clone(),
        stats: stats.clone(),
        mdns,
        airplay: airplay.clone(),
        chromecast: chromecast.clone(),
    };

    let rt = SessionRuntime {
        control,
        capture,
        airplay,
        chromecast,
        recorder,
        stats,
        cfg: cfg.clone(),
    };
    tokio::spawn(session::run(rt));

    let addr: SocketAddr = cfg
        .http_bind
        .parse()
        .with_context(|| format!("parse http_bind {}", cfg.http_bind))?;
    let app = webapi::router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "HTTP API listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn target_from_cfg(t: &TargetConfig) -> SpeakerTarget {
    let target_type = match t.target_type.to_ascii_lowercase().as_str() {
        "chromecast" | "cast" | "googlecast" => TargetType::Chromecast,
        _ => TargetType::Airplay,
    };
    SpeakerTarget {
        name: t.name.clone(),
        ip: t.ip.clone(),
        device_id: t.device_id.clone(),
        port: t.port,
        target_type,
        model: None,
    }
}

fn parse_port(bind: &str) -> Option<u16> {
    bind.rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
}
