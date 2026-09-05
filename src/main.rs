#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod sidebar;
#[cfg(windows)]
mod webview;

#[cfg(windows)]
use anyhow::{Context, bail};
#[cfg(windows)]
use majsoul_max_rs::*;
#[cfg(windows)]
use sidebar::ReloadedSettings;
#[cfg(windows)]
use std::{net::SocketAddr, path::Path, str::FromStr, sync::Arc, time::Duration};
#[cfg(windows)]
use tokio::{
    sync::{mpsc::UnboundedReceiver, oneshot},
    task::JoinHandle,
};
#[cfg(windows)]
use webview::ProxyCommand;

#[cfg(windows)]
fn main() {
    init_trace();
    if let Err(error) = run_application() {
        sidebar::show_error_dialog(&format!("{error:#}"));
    }
}

#[cfg(windows)]
fn run_application() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to start Tokio runtime")?;
    let _guard = runtime.enter();

    let config_hint = Path::new("./liqi_config");
    let settings = Settings::load_config(config_hint)?;
    let config_dir = settings.data_dir().to_path_buf();
    let proxy_addr = settings.proxy_addr.clone();
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let manager_task = runtime.spawn(proxy_manager(config_dir, proxy_addr.clone(), command_rx));

    let webview_result = webview::run(&proxy_addr, Arc::new(settings), command_tx.clone());

    let _ = command_tx.send(ProxyCommand::Shutdown);
    runtime
        .block_on(manager_task)
        .context("Proxy manager panicked")?;
    webview_result
}

#[cfg(windows)]
struct RunningProxy {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
    modder: Option<Arc<Modder>>,
}

#[cfg(windows)]
impl RunningProxy {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        match tokio::time::timeout(Duration::from_secs(2), &mut self.task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => warn!("Proxy stopped with an error: {error}"),
            Ok(Err(error)) => warn!("Proxy task panicked: {error}"),
            Err(_) => {
                self.task.abort();
                let _ = self.task.await;
            }
        }
    }
}

#[cfg(windows)]
fn load_proxy_configuration(config_dir: &Path) -> Result<(Arc<Settings>, Option<Arc<Modder>>)> {
    let mut settings = Settings::load_config(config_dir)?;
    settings.load_protocol()?;
    let settings = Arc::new(settings);
    let modder = if settings.mod_on() {
        info!("Mod worker started");
        let max_data = MaxData::load(settings.data_dir())?;
        let mod_settings = RwLock::new(ModSettings::new(settings.as_ref())?);
        Some(Arc::new(Modder::new(mod_settings, max_data)))
    } else {
        None
    };
    Ok((settings, modder))
}

#[cfg(windows)]
async fn start_proxy(settings: Arc<Settings>, modder: Option<Arc<Modder>>) -> Result<RunningProxy> {
    let proxy_addr = settings.proxy_addr.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running_modder = modder.clone();
    let mut task = tokio::spawn(build_and_start_proxy(settings, modder, async move {
        let _ = shutdown_rx.await;
    }));

    tokio::select! {
        biased;
        result = &mut task => {
            result.context("Proxy task panicked")??;
            bail!("Proxy stopped before accepting connections at {proxy_addr}");
        }
        readiness = wait_for_proxy(&proxy_addr) => {
            if let Err(error) = readiness {
                let _ = shutdown_tx.send(());
                let _ = task.await;
                return Err(error);
            }
        }
    }
    Ok(RunningProxy {
        shutdown: Some(shutdown_tx),
        task,
        modder: running_modder,
    })
}

#[cfg(windows)]
async fn proxy_manager(
    config_dir: std::path::PathBuf,
    browser_proxy_addr: String,
    mut commands: UnboundedReceiver<ProxyCommand>,
) {
    let mut running = None;
    while let Some(command) = commands.recv().await {
        match command {
            ProxyCommand::Reload { response } => {
                let result = reload_proxy(&config_dir, &browser_proxy_addr, &mut running)
                    .await
                    .map_err(|error| error.to_string());
                let _ = response.send(result);
            }
            ProxyCommand::ApplyModPatch(patch) => {
                if let Some(modder) = running.as_ref().and_then(|proxy| proxy.modder.as_ref()) {
                    modder.apply_live_patch(patch).await;
                }
            }
            ProxyCommand::Shutdown => break,
        }
    }
    if let Some(proxy) = running {
        proxy.stop().await;
    }
}

#[cfg(windows)]
async fn reload_proxy(
    config_dir: &Path,
    browser_proxy_addr: &str,
    running: &mut Option<RunningProxy>,
) -> Result<ReloadedSettings> {
    let (settings, modder) = load_proxy_configuration(config_dir)?;
    if settings.proxy_addr != browser_proxy_addr {
        bail!(
            "proxyAddr 已从 {browser_proxy_addr} 改为 {}；WebView2 代理地址无法热切换，请关闭程序后重新打开",
            settings.proxy_addr
        );
    }

    if let Some(proxy) = running.take() {
        proxy.stop().await;
    }
    let mut last_error = None;
    for attempt in 0..2u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = wait_for_proxy_stop(browser_proxy_addr).await;
        match start_proxy(Arc::clone(&settings), modder.clone()).await {
            Ok(replacement) => {
                *running = Some(replacement);
                return Ok(ReloadedSettings { settings });
            }
            Err(error) => {
                warn!("代理启动失败（第 {} 次尝试）: {error}", attempt + 1);
                last_error = Some(error);
            }
        }
    }
    Err(last_error.expect("the retry loop always runs at least once"))
}

#[cfg(windows)]
async fn wait_for_proxy(proxy_addr: &str) -> Result<()> {
    let ping_url = format!("http://{proxy_addr}/ping");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(200))
        .build()
        .context("Failed to build proxy health client")?;
    for _ in 0..50 {
        if proxy_is_ready(&client, &ping_url, proxy_addr).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("Local proxy did not start at {proxy_addr}")
}

#[cfg(windows)]
async fn proxy_is_ready(client: &reqwest::Client, ping_url: &str, proxy_addr: &str) -> bool {
    if let Ok(response) = client.get(ping_url).send().await
        && response.status().is_success()
        && let Ok(body) = response.text().await
        && body.contains("pong")
    {
        return true;
    }
    let Ok(addr) = SocketAddr::from_str(proxy_addr) else {
        return false;
    };
    tokio::net::TcpStream::connect(addr).await.is_ok()
}

#[cfg(windows)]
async fn wait_for_proxy_stop(proxy_addr: &str) -> Result<()> {
    let addr = SocketAddr::from_str(proxy_addr)
        .with_context(|| format!("Invalid proxy address: {proxy_addr}"))?;
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(addr).await.is_err() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("旧代理未能释放监听地址 {proxy_addr}")
}

#[cfg(not(windows))]
fn main() {
    eprintln!("majsoul_max_rs currently supports Windows WebView2 builds only");
}

