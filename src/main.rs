use ai48proxy::{
    config::Config,
    home::{bootstrap, client::supervisor::Supervisor, transport::Connector},
    observability::{HomeLogLayer, LogForwarder},
    reporting::{QueueLimits, ReportingQueue},
};
use anyhow::{Context, Result, ensure};
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

mod cli;
use cli::Args;
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ai48proxy=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(HomeLogLayer)
        .init();
    let args = Args::parse();
    if let Some(url) = &args.healthcheck {
        return cli::healthcheck(url).await;
    }
    if let Some(path) = &args.check_config {
        let cfg = Config::parse(&tokio::fs::read(path).await?)?;
        let unsupported = cfg.unsupported_features();
        ensure!(
            unsupported.is_empty(),
            "unsupported config keys: {}",
            unsupported.join(", ")
        );
        println!(
            "Configuration valid; lifecycle revision {}",
            cfg.lifecycle.lifecycle_config_revision
        );
        return Ok(());
    }
    let jwt = args.bootstrap_token().await?;
    ensure!(
        jwt.is_some() || args.home_host.is_some(),
        "provide HOME_JWT, HOME_JWT_FILE, or --home-host and --home-port with an enrolled identity"
    );
    if args.worker_start_delay > 0 {
        tracing::info!(
            seconds = args.worker_start_delay,
            "waiting before Home startup"
        );
        if !cli::wait_for_start(args.worker_start_delay, shutdown_signal()).await {
            return Ok(());
        }
    }
    let (host, port) = if let Some(jwt) = jwt {
        bootstrap::enroll(&jwt, &args.identity_dir)
            .await
            .map_err(|_| {
                anyhow::anyhow!("Home enrollment failed; check provisioning and identity state")
            })?
    } else {
        (
            args.home_host.context(
                "provide HOME_JWT or --home-host and --home-port with an enrolled identity",
            )?,
            args.home_port.context("missing Home port")?,
        )
    };
    let connector = Connector::from_identity(host, port, &args.identity_dir).await?;
    let reporting = Arc::new(ReportingQueue::open(
        &args.identity_dir.join("reporting"),
        QueueLimits::default(),
    )?);
    let supervisor = Supervisor::new(connector, reporting);
    let logs = LogForwarder::install(&supervisor)?;
    let stop = supervisor.cancellation();
    let signal_stop = stop.clone();
    let signals = tokio::spawn(async move {
        shutdown_signal().await;
        signal_stop.cancel();
    });
    let home = supervisor.connect().await?;
    let config = home.config();
    let bind_host = if config.host.is_empty() {
        "0.0.0.0"
    } else {
        &config.host
    };
    let listener = tokio::net::TcpListener::bind((bind_host, config.port)).await?;
    tracing::info!(address=%listener.local_addr()?,"gateway listening; native API-key forwarding only");
    let cancel = stop.clone();
    let shutdown_home = home.clone();
    let (stopped_tx, mut stopped_rx) = tokio::sync::watch::channel(false);
    let recovery = tokio::spawn(supervisor.clone().run());
    let server = axum::serve(listener, supervisor.router()).with_graceful_shutdown(async move {
        cancel.cancelled().await;
        shutdown_home.stop_admission();
        let _ = stopped_tx.send(true);
    });
    let mut server = tokio::spawn(server.into_future());
    let mut recovery = recovery;
    let mut recovery_result = None;
    let mut server_finished = false;
    tokio::select! {
        result = &mut server => {
            server_finished = true;
            stop.cancel();
            result??;
        },
        result = &mut recovery => {
            recovery_result = Some(result);
            stop.cancel();
        },
        _ = stopped_rx.changed() => {},
    }
    stop.cancel();
    if let Some(current) = supervisor.current() {
        current.stop_admission();
    }
    let shutdown_timeout = supervisor
        .current()
        .map_or(config.shutdown_timeout, |home| {
            home.config().shutdown_timeout
        });
    if !server_finished
        && tokio::time::timeout(shutdown_timeout, &mut server)
            .await
            .is_err()
    {
        if let Some(current) = supervisor.current() {
            current.fence();
        }
        server.abort();
        let _ = server.await;
    }
    let result = match recovery_result {
        Some(result) => result?,
        None => {
            match tokio::time::timeout(config.lifecycle.cancel_bound * 5, &mut recovery).await {
                Ok(result) => result?,
                Err(_) => {
                    if let Some(current) = supervisor.current() {
                        current.fence();
                        let _ = current.retire().await;
                    }
                    recovery.abort();
                    let _ = recovery.await;
                    anyhow::bail!(
                        "Home supervisor shutdown deadline exceeded; durable usage retained"
                    );
                }
            }
        }
    };
    let log_stats = logs.shutdown(config.lifecycle.cancel_bound).await;
    tracing::info!(
        sent = log_stats.sent,
        overflow = log_stats.overflow,
        unavailable = log_stats.unavailable,
        failed = log_stats.failed,
        "Home log forwarding stopped"
    );
    signals.abort();
    let _ = signals.await;
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
