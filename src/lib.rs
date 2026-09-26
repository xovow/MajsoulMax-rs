use anyhow::Context;
use handler::Handler;
use hudsucker::{Proxy, certificate_authority::RcgenAuthority, rcgen::KeyPair, rustls};
use std::{future::Future, net::SocketAddr, str::FromStr, sync::Arc};

mod connections;
mod debug_log;
mod handler;
mod modder;
mod parser;
mod proto;
mod settings;
mod update_schedule;

pub use crate::{
    debug_log::start as start_debug_log,
    modder::{Modder, SaveErrorHandler},
    settings::{
        LiqiUpdatePhase, LiqiUpdateStatus, LiveModPatch, MaxData, ModSettings, Settings,
        UpdateCheckMode, parse_update_interval_minutes, read_settings_file, write_json_setting,
    },
    update_schedule::UpdateCheckSchedule,
};
pub use anyhow::Result;
pub use tokio::sync::RwLock;
pub use tracing_appender::non_blocking::WorkerGuard as DebugLogGuard;

fn generate_ca() -> Result<RcgenAuthority> {
    const KEY_PAIR: &str = include_str!("./ca/hudsucker.key");
    const CA_CERT: &str = include_str!("./ca/hudsucker.cer");
    let key_pair = KeyPair::from_pem(KEY_PAIR).context("Failed to parse key pair")?;
    let issuer = hudsucker::rcgen::Issuer::from_ca_cert_pem(CA_CERT, key_pair)
        .expect("Failed to parse CA certificate");

    let ca = RcgenAuthority::new(issuer, 1_000, rustls::crypto::aws_lc_rs::default_provider());
    Ok(ca)
}

pub async fn build_and_start_proxy<F>(
    settings: Arc<Settings>,
    modder: Option<Arc<Modder>>,
    graceful_shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let ca = generate_ca()?;

    let proxy_addr = SocketAddr::from_str(settings.proxy_addr.as_str())
        .context("Failed to parse proxy address")?;

    let handler = Handler::new(modder);
    let proxy = Proxy::builder()
        .with_addr(proxy_addr)
        .with_ca(ca)
        .with_rustls_connector(rustls::crypto::aws_lc_rs::default_provider())
        .with_http_handler(handler.clone())
        .with_websocket_handler(handler)
        .with_graceful_shutdown(graceful_shutdown)
        .build()
        .context("Failed to build proxy")?;

    proxy.start().await.context("Failed to start proxy")
}
