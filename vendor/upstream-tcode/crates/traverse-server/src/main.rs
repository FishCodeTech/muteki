use std::{path::PathBuf, time::SystemTime};

use tcode_traverse_server::{Config, Server, config::TEMPLATE};
use tracing_subscriber::EnvFilter;

const DEV_HTTP_BIND: &str = "127.0.0.1:8080";

fn print_usage() {
    println!(
        "Usage:\n  tcode-traverse --config PATH\n  tcode-traverse --dev [--data-dir DIR] [--http-bind ADDR:PORT]\n  tcode-traverse --print-default-config\n\n--config runs the instance described by a TOML file (--print-default-config\nwrites a commented template). --dev serves plain HTTP on {DEV_HTTP_BIND} with\nthe store in ./tcode-traverse-dev, no TLS, no QUIC address discovery and no\nmetrics listener. Logging follows RUST_LOG.\n\nOptions:\n  -h, --help    Print this help"
    );
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    if let Err(error) = run(std::env::args().skip(1).collect()) {
        eprintln!("tcode-traverse: {error}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut config_path: Option<PathBuf> = None;
    let mut dev = false;
    let mut data_dir: Option<PathBuf> = None;
    let mut http_bind: Option<String> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            "--print-default-config" => {
                print!("{TEMPLATE}");
                return Ok(());
            }
            "--config" => config_path = Some(iter.next().ok_or("--config needs a path")?.into()),
            "--dev" => dev = true,
            "--data-dir" => data_dir = Some(iter.next().ok_or("--data-dir needs a path")?.into()),
            "--http-bind" => http_bind = Some(iter.next().ok_or("--http-bind needs an address")?),
            other => return Err(format!("unknown argument {other}; see --help")),
        }
    }
    let (config, updated_at) = match (config_path, dev) {
        (Some(path), false) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let config = Config::parse(&text)
                .map_err(|error| format!("invalid config {}: {error}", path.display()))?;
            let updated_at = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or_else(|_| SystemTime::now());
            (config, updated_at)
        }
        (None, true) => {
            let bind = http_bind
                .as_deref()
                .unwrap_or(DEV_HTTP_BIND)
                .parse()
                .map_err(|error| format!("invalid --http-bind: {error}"))?;
            let data_dir = data_dir.unwrap_or_else(|| PathBuf::from("tcode-traverse-dev"));
            (Config::dev(data_dir, bind), SystemTime::now())
        }
        (Some(_), true) => return Err("--config and --dev are exclusive".into()),
        (None, false) => {
            print_usage();
            return Err("--config or --dev is required".into());
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let mut server = Server::spawn(config, updated_at)
            .await
            .map_err(|error| error.to_string())?;
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
            _ = server.join() => {}
        }
        server.shutdown().await;
        Ok(())
    })
}
