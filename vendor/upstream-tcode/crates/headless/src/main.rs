use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use tcode_client::pairing::{PairInvite, pair_url, parse_pair_url};
use tcode_core::settings::{Settings, provider_key};
use tcode_protocol::{TraverseLookupState, TraverseManifestState, TraverseSourceStatus};
use tcode_runtime::pipe::{HostServices, spawn_host};
use tcode_services::store::{Migration, MigrationPhase, MigrationProgress, SessionStore};
use tcode_traverse::browser::{BrowserConfig, StaticBundle, check_bind, serve, set_password};
use tcode_traverse::identity::write_private;
use tcode_traverse::lan::DEFAULT_PORT;
use tcode_traverse::manifest::ManifestSource;
use tcode_traverse::native_host::default_device_name;
use tcode_traverse::{HostConfig, HostMux, Invitation, TraverseHost};

#[cfg(feature = "web")]
const STATIC_BUNDLE: Option<StaticBundle> = Some(&[
    ("/index.html", include_bytes!("../../web/dist/index.html")),
    ("/auth.mjs", include_bytes!("../../web/dist/auth.mjs")),
    (
        "/tcode_web.js",
        include_bytes!("../../web/dist/tcode_web.js"),
    ),
    (
        "/tcode_web_bg.wasm",
        include_bytes!("../../web/dist/tcode_web_bg.wasm"),
    ),
]);
#[cfg(not(feature = "web"))]
const STATIC_BUNDLE: Option<StaticBundle> = None;

/// The browser listener stays on loopback unless asked otherwise; devices
/// reach the machine through Traverse.
const DEFAULT_BROWSER_LISTEN: &str = "127.0.0.1:47420";
/// The current invitation, for `pair` to print; absent while none is valid.
const INVITATION_FILE: &str = "invitation.json";
/// How often a running migration prints its progress.
const MIGRATION_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Set by SIGINT or SIGTERM: cancels a startup migration, then stops `serve`.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn main() {
    env_logger::init();
    if let Err(error) = run(std::env::args().skip(1).collect()) {
        eprintln!("tcode-headless: {error}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None | Some("--help" | "-h") => {
            print_usage();
            Ok(())
        }
        Some("serve") => serve_command(&args[1..]),
        Some("pair") => pair_command(&args[1..]),
        Some("set-password") => set_password_command(&args[1..]),
        Some(command) => Err(format!("unknown command {command:?}; use --help")),
    }
}

fn print_usage() {
    println!(
        "Usage:\n  tcode-headless serve [--name NAME] [--data-dir DIR] [--traverse official|off|URL]... [--port PORT] [--browser-listen ADDR:PORT] [--password PASSWORD]\n  tcode-headless set-password [--data-dir DIR] [--password PASSWORD] [--revoke-tokens]\n  tcode-headless pair [--data-dir DIR]\n\nserve starts this machine on Traverse for native devices and, for browsers,\na plain HTTP listener on {DEFAULT_BROWSER_LISTEN} (--browser-listen binds it\nelsewhere; --listen is accepted as an alias). The browser signs in with a\npassword, set on first open or with --password / TCODE_PASSWORD; a bind\nbeyond loopback is refused until one exists. --traverse names a relay and\ndiscovery service to publish to: official (the default) or the base URL of a\nself-hosted instance; repeat it to publish to several, or give off alone for\nnone (devices then find it on the LAN, or at an address typed on the device). The machine binds UDP port\n{DEFAULT_PORT} for devices (--port binds another) and advertises it on the\nLAN as _tcode._udp, so paired devices on the same network find it again\nwithout Traverse.\n\npair prints the current invitation link and QR: serve keeps {INVITATION_FILE}\ncurrent, whether the invitation was minted at startup or from a paired\ndevice, and removes it once it is used or expires. Scanning or pasting the\nlink is the whole pairing; an invitation lasts five minutes and admits one\ndevice. A new one comes from the logged-in browser's Settings → Remote or a\nrestart.\n\nOptions:\n  -h, --help    Print this help"
    );
}

/// Every `--traverse` value in order: `official`, a self-hosted base URL, or
/// `off` alone for none. Without the option the machine uses the official
/// service.
fn parse_traverse(values: Vec<String>) -> Result<Vec<ManifestSource>, String> {
    if values.is_empty() {
        return Ok(vec![ManifestSource::Official]);
    }
    if values.iter().any(|value| value == "off") {
        return if values.len() == 1 {
            Ok(Vec::new())
        } else {
            Err("--traverse off cannot be combined with another --traverse".into())
        };
    }
    let mut sources = Vec::with_capacity(values.len());
    for value in values {
        let source = match value.as_str() {
            "official" => ManifestSource::Official,
            url => url::Url::parse(url)
                .map(ManifestSource::Custom)
                .map_err(|error| format!("invalid --traverse value {url:?}: {error}"))?,
        };
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    Ok(sources)
}

fn serve_command(args: &[String]) -> Result<(), String> {
    let browser_listen = option_value(args, "--browser-listen")
        .or_else(|| option_value(args, "--listen"))
        .unwrap_or_else(|| DEFAULT_BROWSER_LISTEN.to_owned())
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid --browser-listen address: {error}"))?;
    let name = option_value(args, "--name").unwrap_or_else(default_device_name);
    let data_dir = option_value(args, "--data-dir").map(PathBuf::from);
    let traverse = parse_traverse(option_values(args, "--traverse"))?;
    let port = match option_value(args, "--port") {
        Some(port) => port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("invalid --port value {port:?}: expected 1-65535"))?,
        None => DEFAULT_PORT,
    };
    reject_unknown_options(
        args,
        &[
            "--listen",
            "--browser-listen",
            "--name",
            "--data-dir",
            "--password",
            "--traverse",
            "--port",
        ],
    )?;
    let store = SessionStore::open_host(data_dir)
        .map_err(|error| format!("could not open session store: {error}"))?;
    let remote_data_dir = store.root().clone();
    // The password lives in the data dir, which may still have to move in.
    install_interrupt_handler();
    if migrate_store(&store)? == Migration::Cancelled {
        return Ok(());
    }
    if let Some(selection) = std::env::var_os("TCODE_AGENTS") {
        let selection = selection
            .into_string()
            .map_err(|_| "TCODE_AGENTS must be valid Unicode")?;
        let settings_store = tcode_services::settings::SettingsStore::new(remote_data_dir.clone());
        let mut settings = settings_store
            .try_load()
            .map_err(|error| format!("could not load provider settings: {error}"))?;
        select_agents(&mut settings, &selection)?;
        settings_store
            .save(&settings)
            .map_err(|error| format!("could not save provider selection: {error}"))?;
    }
    if let Some(password) =
        option_value(args, "--password").or_else(|| std::env::var("TCODE_PASSWORD").ok())
    {
        set_password(&remote_data_dir, &password, false).map_err(|error| error.to_string())?;
    }
    // Nothing else starts for a bind the listener would refuse anyway.
    check_bind(browser_listen, &remote_data_dir).map_err(|error| error.to_string())?;
    let mut services = HostServices {
        background_startup_probes: true,
        ai_title_generation: true,
        ..HostServices::default()
    };
    if let Err(error) = services.start_mcp_servers(false) {
        eprintln!("tcode-headless: MCP servers unavailable: {error}");
    }
    let host =
        spawn_host(store, services).map_err(|error| format!("machine startup failed: {error}"))?;
    let mux = HostMux::new(host.attachments.clone(), host.from_host.clone())
        .with_original_backend(std::sync::Arc::new(host.clone()));
    let relayed = !traverse.is_empty();
    let traverse_host = Arc::new(
        TraverseHost::start(
            mux.clone(),
            HostConfig {
                host_name: name.clone(),
                data_dir: remote_data_dir.clone(),
                traverse,
                pairing_enabled: true,
                bind_port: Some(port),
            },
        )
        .map_err(|error| format!("could not start Traverse on UDP port {port}: {error}"))?,
    );
    // The file follows every change for as long as the host runs; the
    // thread ends with the host's event stream. A copy left by a serve that
    // did not shut down goes first.
    let events = traverse_host.invitation_events();
    sync_invitation_file(&remote_data_dir, None, &[])?;
    let invitation_dir = remote_data_dir.clone();
    let addressed = Arc::downgrade(&traverse_host);
    std::thread::spawn(move || {
        while let Ok(invitation) = events.recv_blocking() {
            let addrs = addressed
                .upgrade()
                .map(|host| direct_addrs(&host))
                .unwrap_or_default();
            if let Err(error) = sync_invitation_file(&invitation_dir, invitation.as_ref(), &addrs) {
                eprintln!("tcode-headless: {error}");
            }
        }
    });
    let traverse_events = traverse_host.traverse_events();
    std::thread::spawn(move || {
        log_traverse_status(std::iter::from_fn(|| traverse_events.recv_blocking().ok()))
    });
    let hosting = traverse_host.clone();
    let server = serve(
        mux.clone(),
        BrowserConfig {
            listen: browser_listen,
            host_name: name,
            data_dir: remote_data_dir.clone(),
            static_bundle: STATIC_BUNDLE,
            hosting: Some(Arc::new(move |action| hosting.hosting(action))),
        },
    )
    .map_err(|error| format!("could not listen for browsers: {error}"))?;
    println!("Machine id: {}", traverse_host.endpoint_id());
    println!("UDP port: {port}");
    if relayed {
        // An invite minted before the relay is known would name none; wait
        // briefly, never indefinitely.
        traverse_host.wait_online(Duration::from_secs(5));
    }
    if traverse_host.pairing_enabled() {
        let invitation = traverse_host.new_invitation();
        print_invite(
            &invitation.invite,
            invitation.remaining().as_secs(),
            &direct_addrs(&traverse_host),
        )?;
    } else {
        println!("Pairing disabled; enable Accept new devices from a paired client");
    }
    println!(
        "{}",
        if server.password_configured() {
            "Password protected"
        } else {
            "Set a password on first open"
        }
    );
    for space in traverse_host.spaces() {
        if let Some(link) = space.link {
            println!("Space {}: {link}", space.name);
        }
    }
    println!("Browser: http://{}/", server.local_addr());
    println!("Press Ctrl-C to stop");
    wait_for_interrupt();

    let shutdown_connection = mux.attach(
        tcode_protocol::Principal::Full,
        tcode_client::attachment::AttachmentPolicy::network(),
    );
    let shutdown_id = 1_u64;
    let shutdown_line = serde_json::to_string(&tcode_protocol::ClientMessage {
        principal: None,
        key: None,
        id: shutdown_id,
        payload: tcode_protocol::ClientPayload::Command(
            tcode_protocol::Command::ShutdownAllAndFlush,
        ),
    })
    .map_err(|error| error.to_string())?;
    shutdown_connection
        .to_host
        .send_blocking(shutdown_line)
        .map_err(|error| format!("could not stop this machine: {error}"))?;
    while let Ok(line) = shutdown_connection.from_host.recv_blocking() {
        let Ok(message) = serde_json::from_str::<tcode_protocol::HostMessage>(line.trim_end())
        else {
            continue;
        };
        if matches!(message, tcode_protocol::HostMessage::Ack { id, .. } if id == shutdown_id) {
            break;
        }
    }
    let _ = std::fs::remove_file(remote_data_dir.join(INVITATION_FILE));
    server.shutdown();
    if let Ok(traverse_host) = Arc::try_unwrap(traverse_host) {
        traverse_host.shutdown();
    }
    host.to_host.close();
    let _ = host.stopped.recv_blocking();
    Ok(())
}

/// Move an older build's data dir in and its threads into `tcode.db` before
/// the host starts, printing progress; an interrupt cancels it, losing
/// nothing, and the next start continues.
fn migrate_store(store: &SessionStore) -> Result<Migration, String> {
    let open_error = |error: std::io::Error| format!("could not open session store: {error}");
    let needed = store.needs_migration().map_err(open_error)?;
    if !needed {
        return Ok(Migration::Completed);
    }
    match store.pending_relocation().map_err(open_error)? {
        Some(previous) => println!(
            "Moving {} to {}, then migrating any older threads into tcode.db. Ctrl-C cancels; \
             the next start continues.",
            previous.display(),
            store.root().display()
        ),
        None => println!(
            "Migrating threads into {}; each copy is verified before the original files are \
             removed. Ctrl-C cancels.",
            store.root().join("tcode.db").display()
        ),
    }
    let mut last: Option<(MigrationPhase, Instant)> = None;
    let outcome = store
        .migrate(
            |progress| {
                let due = last.is_none_or(|(phase, at)| {
                    phase != progress.phase || at.elapsed() >= MIGRATION_REPORT_INTERVAL
                });
                if due {
                    println!("{}", migration_line(&progress));
                    last = Some((progress.phase, Instant::now()));
                }
            },
            &INTERRUPTED,
        )
        .map_err(|error| format!("migration failed: {error}"))?;
    match outcome {
        Migration::Completed => println!("Migration complete"),
        Migration::Cancelled => {
            println!("Migration cancelled; nothing was lost, and the next start continues")
        }
    }
    Ok(outcome)
}

fn migration_line(progress: &MigrationProgress) -> String {
    let (phase, items) = match progress.phase {
        MigrationPhase::Relocating => ("moving", "entries"),
        MigrationPhase::Scanning => ("scanning", "threads"),
        MigrationPhase::Importing => ("importing", "threads"),
        MigrationPhase::Verifying => ("verifying", "threads"),
        MigrationPhase::Publishing => ("publishing", "threads"),
        MigrationPhase::Archiving => ("archiving", "threads"),
    };
    let mib = |bytes: u64| bytes as f64 / (1024. * 1024.);
    format!(
        "{phase}: {}/{} {items}, {:.1}/{:.1} MiB",
        progress.threads_done,
        progress.threads_total,
        mib(progress.bytes_done),
        mib(progress.bytes_total)
    )
}

fn set_password_command(args: &[String]) -> Result<(), String> {
    let revoke = args.iter().any(|arg| arg == "--revoke-tokens");
    let values: Vec<_> = args
        .iter()
        .filter(|arg| arg.as_str() != "--revoke-tokens")
        .cloned()
        .collect();
    reject_unknown_options(&values, &["--data-dir", "--password"])?;
    let password = option_value(&values, "--password")
        .or_else(|| std::env::var("TCODE_PASSWORD").ok())
        .ok_or("supply --password or TCODE_PASSWORD")?;
    let store = SessionStore::open_host(option_value(&values, "--data-dir").map(PathBuf::from))
        .map_err(|error| error.to_string())?;
    // The move would stop at the password file this writes.
    if let Some(previous) = store
        .pending_relocation()
        .map_err(|error| error.to_string())?
    {
        return Err(format!(
            "{} has not been moved into {} yet; run `tcode-headless serve` once first",
            previous.display(),
            store.root().display()
        ));
    }
    set_password(store.root(), &password, revoke).map_err(|error| error.to_string())?;
    println!(
        "Password changed. {}",
        if revoke {
            "Existing tokens revoked."
        } else {
            "Existing tokens kept."
        }
    );
    Ok(())
}

/// `serve` keeps the current invitation here for `pair` to print, with the
/// machine's addresses for a person to type where nothing finds it; the
/// link itself names none.
#[derive(serde::Serialize, serde::Deserialize)]
struct InvitationFile {
    expires_unix: u64,
    invite: String,
    #[serde(default)]
    addrs: Vec<String>,
}

/// How long the sources may take to settle before their first summary is
/// printed anyway.
const TRAVERSE_SETTLE: Duration = Duration::from_secs(30);

/// Print each Traverse source's summary once the sources settle — every
/// lookup checked and a home relay chosen — and again whenever it changes.
/// Latencies and check times alone are not a change.
fn log_traverse_status(events: impl IntoIterator<Item = Vec<TraverseSourceStatus>>) {
    let started = Instant::now();
    let mut printed = None;
    for status in events {
        let settled = status.iter().all(|source| {
            source
                .lookups
                .iter()
                .all(|lookup| lookup.state != TraverseLookupState::Pending)
        }) && (status.iter().all(|source| source.relays.is_empty())
            || status
                .iter()
                .any(|source| source.relays.iter().any(|relay| relay.home)));
        if !settled && started.elapsed() < TRAVERSE_SETTLE {
            continue;
        }
        let shape: Vec<_> = status.iter().map(traverse_shape).collect();
        if printed.as_ref() == Some(&shape) {
            continue;
        }
        for source in &status {
            println!("{}", traverse_summary(source));
        }
        printed = Some(shape);
    }
}

/// What a summary line says, less the numbers that move on their own.
fn traverse_shape(source: &TraverseSourceStatus) -> TraverseSourceStatus {
    let mut shape = source.clone();
    shape.manifest.fetched_unix = None;
    for relay in &mut shape.relays {
        relay.latency_ms = None;
    }
    for lookup in &mut shape.lookups {
        lookup.checked_unix = None;
    }
    shape
}

/// `Traverse official: manifest live · 3 relays · best 42 ms (eu) · home eu · lookup ok`.
fn traverse_summary(source: &TraverseSourceStatus) -> String {
    let mut parts = vec![match &source.manifest.error {
        Some(error) => format!(
            "manifest {} (last fetch failed: {error})",
            manifest_word(source.manifest.state)
        ),
        None => format!("manifest {}", manifest_word(source.manifest.state)),
    }];
    if !source.relays.is_empty() {
        parts.push(format!("{} relays", source.relays.len()));
        if let Some(best) = source
            .relays
            .iter()
            .filter(|relay| relay.latency_ms.is_some())
            .min_by_key(|relay| relay.latency_ms)
        {
            parts.push(format!(
                "best {} ms ({})",
                best.latency_ms.unwrap_or_default(),
                relay_label(best)
            ));
        }
        for relay in source.relays.iter().filter(|relay| relay.home) {
            parts.push(match &relay.error {
                Some(error) => format!("home {} failing: {error}", relay_label(relay)),
                None => format!("home {}", relay_label(relay)),
            });
        }
    }
    for lookup in &source.lookups {
        parts.push(match (lookup.state, &lookup.error) {
            (TraverseLookupState::Pending, _) => "lookup unchecked".to_owned(),
            (TraverseLookupState::Ok, _) => "lookup ok".to_owned(),
            (TraverseLookupState::Failed, error) => format!(
                "lookup {} failed: {}",
                lookup.url,
                error.as_deref().unwrap_or_default()
            ),
        });
    }
    format!("Traverse {}: {}", source.source, parts.join(" · "))
}

fn manifest_word(state: TraverseManifestState) -> &'static str {
    match state {
        TraverseManifestState::Live => "live",
        TraverseManifestState::Cached => "cached",
        TraverseManifestState::Bundled => "bundled",
        TraverseManifestState::Failed => "failed",
    }
}

/// The manifest's region for a relay, else its host.
fn relay_label(relay: &tcode_protocol::TraverseRelayStatus) -> String {
    relay.region.clone().unwrap_or_else(|| {
        url::Url::parse(&relay.url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| relay.url.clone())
    })
}

fn direct_addrs(host: &TraverseHost) -> Vec<String> {
    host.direct_addrs()
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Write the invitation in effect, or remove the file when there is none.
fn sync_invitation_file(
    data_dir: &Path,
    invitation: Option<&Invitation>,
    addrs: &[String],
) -> Result<(), String> {
    let path = data_dir.join(INVITATION_FILE);
    let Some(invitation) = invitation else {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not remove {}: {error}", path.display())),
        };
    };
    let file = InvitationFile {
        expires_unix: now_unix() + invitation.remaining().as_secs(),
        invite: invitation.url(),
        addrs: addrs.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
    write_private(&path, &bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn pair_command(args: &[String]) -> Result<(), String> {
    reject_unknown_options(args, &["--data-dir"])?;
    let store = SessionStore::open_host(option_value(args, "--data-dir").map(PathBuf::from))
        .map_err(|error| error.to_string())?;
    let path = store.root().join(INVITATION_FILE);
    let file: Option<InvitationFile> = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let valid = file.filter(|file| file.expires_unix > now_unix());
    let Some(file) = valid else {
        return Err("no valid invitation; the logged-in browser can create one from Settings → Remote, or restart serve".into());
    };
    let invite = parse_pair_url(&file.invite).ok_or("invalid invitation file")?;
    print_invite(&invite, file.expires_unix - now_unix(), &file.addrs)
}

/// `addrs` are printed for a person to read, never put in the link.
fn print_invite(invite: &PairInvite, remaining_secs: u64, addrs: &[String]) -> Result<(), String> {
    let url = pair_url(invite);
    let qr = QrCode::new(url.as_bytes()).map_err(|error| error.to_string())?;
    println!("Invitation (scan the QR or paste the link; one device, five minutes):");
    println!("Expires in: {remaining_secs} seconds");
    match &invite.relay {
        Some(relay) => println!("Relay: {relay}"),
        None => println!("Relay: none (LAN only)"),
    }
    println!("Addresses: {}", addrs.join(", "));
    println!("{url}");
    println!("{}", qr.render::<Dense1x2>().quiet_zone(true).build());
    Ok(())
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    option_values(args, name).into_iter().next()
}

fn select_agents(settings: &mut Settings, selection: &str) -> Result<(), String> {
    let names: Vec<_> = selection
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    for name in &names {
        if !agent::ProviderKind::NATIVE
            .iter()
            .any(|kind| provider_key(*kind) == *name)
        {
            return Err(format!("unknown TCODE_AGENTS entry {name:?}"));
        }
    }
    for kind in agent::ProviderKind::NATIVE {
        settings.provider_mut(kind).enabled = names.contains(&provider_key(kind));
    }
    for profile in settings.profiles.values_mut() {
        if profile.kind != agent::ProviderKind::Acp && !names.contains(&provider_key(profile.kind))
        {
            profile.settings.enabled = false;
        }
    }
    Ok(())
}

/// Every value given for a repeatable option, in order.
fn option_values(args: &[String], name: &str) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .collect()
}

fn reject_unknown_options(args: &[String], options_with_values: &[&str]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        if options_with_values.contains(&args[index].as_str()) {
            if index + 1 >= args.len() {
                return Err(format!("{} requires a value", args[index]));
            }
            index += 2;
        } else {
            return Err(format!("unknown option {:?}", args[index]));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn install_interrupt_handler() {
    type SignalHandler = extern "C" fn(i32);
    unsafe extern "C" {
        fn signal(signal: i32, handler: SignalHandler) -> SignalHandler;
    }
    extern "C" fn handle_interrupt(_: i32) {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    // SAFETY: installs process-global handlers with the C ABI expected by
    // signal(3); the handler performs only a lock-free atomic store.
    unsafe {
        signal(SIGINT, handle_interrupt);
        signal(SIGTERM, handle_interrupt);
    }
}

/// Console interrupts keep their default effect: a migration they end is
/// discarded by the next start, and the sources are untouched until it
/// publishes.
#[cfg(not(unix))]
fn install_interrupt_handler() {}

#[cfg(unix)]
fn wait_for_interrupt() {
    while !INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(not(unix))]
fn wait_for_interrupt() {
    use std::io::Read as _;
    let _ = std::io::stdin().read(&mut [0_u8]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_agent_selection_updates_native_cards_and_preserves_configuration() {
        use agent::ProviderKind;
        let mut settings: Settings = serde_json::from_str(
            r#"{"providers":{"claude":{"binary_path":"/custom/claude","enabled":true}},
                "profiles":{"custom-claude":{"kind":"claude_code","enabled":true}},
                "acp_agents":{"custom":{"id":"custom","name":"Custom",
                    "launch":{"kind":"custom","command":"/data/bin/custom-acp"}}}}"#,
        )
        .unwrap();
        select_agents(&mut settings, " codex,pi,codex ").unwrap();
        assert!(settings.provider(ProviderKind::Codex).enabled);
        assert!(settings.provider(ProviderKind::Pi).enabled);
        for kind in [
            ProviderKind::ClaudeCode,
            ProviderKind::OpenCode,
            ProviderKind::Cursor,
            ProviderKind::Grok,
        ] {
            assert!(!settings.provider(kind).enabled);
        }
        assert_eq!(
            settings.provider(ProviderKind::ClaudeCode).binary_path,
            Some(PathBuf::from("/custom/claude"))
        );
        assert!(!settings.profiles["custom-claude"].settings.enabled);
        assert!(settings.acp_agents["custom"].enabled);

        select_agents(&mut settings, "").unwrap();
        assert!(
            ProviderKind::NATIVE
                .into_iter()
                .all(|kind| !settings.provider(kind).enabled)
        );
        assert!(settings.acp_agents["custom"].enabled);
        let before = settings.clone();
        assert!(select_agents(&mut settings, "codex,typo").is_err());
        assert_eq!(settings, before);
    }

    #[test]
    fn traverse_flags_list_the_sources_to_publish_to() {
        let parse = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            parse_traverse(option_values(&args, "--traverse"))
        };
        let custom = |url: &str| ManifestSource::Custom(url::Url::parse(url).unwrap());
        assert_eq!(parse(&[]).unwrap(), [ManifestSource::Official]);
        assert_eq!(
            parse(&["--name", "Desk", "--traverse", "official"]).unwrap(),
            [ManifestSource::Official]
        );
        assert_eq!(parse(&["--traverse", "off"]).unwrap(), []);
        assert_eq!(
            parse(&["--traverse", "https://a.example/"]).unwrap(),
            [custom("https://a.example/")]
        );
        assert_eq!(
            parse(&[
                "--traverse",
                "official",
                "--port",
                "5000",
                "--traverse",
                "https://a.example/",
                "--traverse",
                "https://b.example/",
            ])
            .unwrap(),
            [
                ManifestSource::Official,
                custom("https://a.example/"),
                custom("https://b.example/")
            ]
        );
        assert!(parse(&["--traverse", "off", "--traverse", "official"]).is_err());
        assert!(parse(&["--traverse", "not a url"]).is_err());
    }
}
