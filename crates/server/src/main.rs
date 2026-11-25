use anyhow::Context;
use clap::Parser;
use muteki_server::{AppState, app, store::Store};
use std::{future::IntoFuture, net::IpAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(version, about = "Muteki local service")]
struct Options {
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    /// Address the web UI listens on; a non-loopback UI also requires a password.
    #[arg(long, default_value = "127.0.0.1")]
    ui_host: IpAddr,
    #[arg(long, default_value_t = 8100)]
    port: u16,
    #[arg(long, default_value = ".state")]
    state_dir: PathBuf,
    #[arg(long)]
    import_from: Option<PathBuf>,
    #[arg(long, requires = "import_from")]
    import_only: bool,
    /// Dedicated TLS-only endpoint for outbound remote Runner connections.
    #[arg(long, requires_all = ["runner_tls_cert", "runner_tls_key"])]
    runner_tls_listen: Option<std::net::SocketAddr>,
    #[arg(long, requires = "runner_tls_listen")]
    runner_tls_cert: Option<PathBuf>,
    #[arg(long, requires = "runner_tls_listen")]
    runner_tls_key: Option<PathBuf>,
}

/// GUI-spawned servers often inherit a 256 soft (and sometimes hard) NOFILE
/// limit; raising both keeps native engine spawns from failing with EMFILE.
#[cfg(unix)]
fn raise_nofile_soft_limit(min_soft: u64) {
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let want = min_soft as libc::rlim_t;
        if lim.rlim_max != libc::RLIM_INFINITY && lim.rlim_max < want {
            lim.rlim_max = want;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                let _ = libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
            }
        }
        let ceiling = if lim.rlim_max == libc::RLIM_INFINITY {
            want
        } else {
            want.min(lim.rlim_max)
        };
        if lim.rlim_cur >= ceiling {
            return;
        }
        lim.rlim_cur = ceiling;
        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    raise_nofile_soft_limit(65_536);
    if muteki_server::runtime_guardian::entry()? { return Ok(()); }
    if muteki_server::src_native_hook::entry().await? { return Ok(()); }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "muteki_server=info".into()),
        )
        .init();
    if std::env::args().nth(1).as_deref() == Some("runner") { return muteki_server::runner::entry().await; }
    if let Some(code)=muteki_server::system_api::management_cli_entry().await{std::process::exit(code);}
    let options = if std::env::args().nth(1).as_deref()==Some("web"){Options::parse_from(std::env::args().take(1).chain(std::env::args().skip(2)))}else{Options::parse()};
    if !options.host.is_loopback() {
        anyhow::bail!("The HTTP API must bind loopback. Expose Web through the Web gateway instead.");
    }
    let exposed = !options.host.is_loopback() || !options.ui_host.is_loopback();
    std::fs::create_dir_all(&options.state_dir).context("Create local state directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&options.state_dir)?.permissions().mode() & 0o777 != 0o700 {
            std::fs::set_permissions(&options.state_dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    muteki_server::shell_env::init_from_cache(&options.state_dir);
    muteki_server::host_process::sync_runtime_caches();
    let store =
        Store::open(options.state_dir.join("muteki.sqlite")).context("Open local database")?;
    muteki_server::legacy_chat_import::upgrade_attachment_references(&store).context("Upgrade imported attachment references")?;
    muteki_server::runtime_api::adopt_discovered_agents(&store).context("Keep discovered agent runtimes added")?;
    store.put(
        "system",
        "state_dir",
        &serde_json::json!(options.state_dir.canonicalize()?),
    )?;
    if let Some(source) = options.import_from {
        muteki_server::account_material::recover(&store,&options.state_dir).context("Recover credential material transactions")?;
        let summary = muteki_server::import::run(&source, &options.state_dir, &store)?;
        let history = muteki_server::history_import::run(&source, &options.state_dir, &store)?;
        let chat = muteki_server::legacy_chat_import::run(&options.state_dir, &store)?;
        println!(
            "{}",
            serde_json::json!({"credentials":summary,"history":history,"chat":chat})
        );
        if options.import_only {
            return Ok(());
        }
    }
    let access = muteki_server::access_api::Access::from_env(
        &options.state_dir,
        store.installation_id()?,
        exposed,
    )?;
    let bind = if options.host.is_loopback() { options.ui_host } else { options.host };
    access.check_exposure(&bind.to_string())?;
    let state = Arc::new(AppState::new(store));
    muteki_server::network_proxy::refresh(&state);
    muteki_server::runner::configure_local(&options.state_dir.canonicalize()?).map_err(anyhow::Error::msg)?;
    muteki_server::runner::supervise_local(options.state_dir.canonicalize()?);
    let runner_listener = if let Some(address)=options.runner_tls_listen {
        let listener = muteki_server::runner_remote::transport::Listener::bind(address,options.runner_tls_cert.as_deref().unwrap(),options.runner_tls_key.as_deref().unwrap()).await?;
        state.runner_listener.set_external(address);
        Some(listener)
    } else {
        let saved=state.clone();
        tokio::spawn(async move {saved.runner_listener.start_saved(saved.clone()).await;});
        None
    };
    let listener = tokio::net::TcpListener::bind((options.host, options.port))
        .await
        .context("Bind local service")?;
    let address = listener.local_addr()?;
    store_port(&state, address.port())?;
    if std::env::var("MUTEKI_DESKTOP_SHELL_DISCOVERY_PENDING").ok().as_deref() == Some("1") {
        muteki_server::host_process::spawn_background_refresh(
            Some(state.clone()),
            options.state_dir.clone(),
            std::time::Duration::from_secs(10),
        );
    } else {
        muteki_server::host_process::spawn_background_refresh(
            Some(state.clone()),
            options.state_dir.clone(),
            std::time::Duration::ZERO,
        );
    }
    if !state.admission.draining() {
        let activation=state.clone();
        tokio::spawn(async move {if let Err(error)=muteki_server::lifecycle::activate(activation).await{tracing::error!(%error,"Service activation failed");}});
    }
    tracing::info!(%address,"Muteki Rust listener bound");
    let service = axum::serve(
        listener,
        app(state.clone(), access).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .into_future();
    tokio::select! {
        result = service => result?,
        result = async {
            if let Some(listener)=runner_listener {axum::serve(listener,muteki_server::runner_remote::transport::router(state.clone())).await}
            else {std::future::pending().await}
        } => result?,
        _ = tokio::signal::ctrl_c() => {},
    }
    state.extension_host.shutdown().await;
    state.agents.close_all().await;
    state.computer.close().await;
    state.browser.close().await;
    if let Some(registry) = state.plugins.opened() {
        registry.close().await;
    }
    Ok(())
}

fn store_port(state: &Arc<AppState>, port: u16) -> anyhow::Result<()> {
    state
        .store
        .put("system", "port", &serde_json::json!(port))
        .context("Record the bound service port")
}
