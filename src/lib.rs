use anyhow::Context;
use handler::Handler;
use hudsucker::{Proxy, certificate_authority::RcgenAuthority, rcgen::KeyPair, rustls};
use std::{future::Future, net::SocketAddr, str::FromStr, sync::Arc};

mod handler;
mod modder;
mod parser;
mod proto;
mod settings;

pub use crate::{
    modder::Modder,
    settings::{LiqiUpdatePhase, LiqiUpdateStatus, LiveModPatch, MaxData, ModSettings, Settings},
};
pub use anyhow::Result;
pub use tokio::sync::RwLock;
pub use tracing::{info, warn};

pub fn init_trace() {
    let timer = tracing_subscriber::fmt::time::ChronoLocal::new("%H:%M:%S%.3f".to_string());
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::WARN.into())
        .from_env()
        .unwrap_or_default()
        .add_directive("majsoul_max_rs=info".parse().unwrap_or_default());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer)
        .compact()
        .init();
}

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
