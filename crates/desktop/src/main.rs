//! NSH AppKit shell hosting Muteki's original React/Next frontend.
use std::{path::Path, process::ExitCode};
const APP_NAME: &str = "Muteki";
#[allow(dead_code)]
mod shell_env;
#[cfg(target_os = "macos")]
mod attachments;
#[cfg(target_os = "macos")]
mod backend;
#[cfg(target_os = "macos")]
mod storage;
#[cfg(target_os = "macos")]
mod bridge;
#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "macos")]
mod speech;
#[cfg(target_os = "macos")]
mod connections;
#[cfg(target_os = "macos")]
mod notifications;
#[cfg(target_os = "macos")]
mod policy;
#[cfg(target_os = "macos")]
mod update_files;
#[cfg(target_os = "macos")]
mod update_runner;
#[cfg(target_os = "macos")]
mod updates;

#[cfg(target_os = "macos")]
use nsh_core::prelude::*;

#[cfg(target_os = "macos")]
#[component]
fn MutekiApp() -> impl IntoElement {
    nsh_core::element::desktop::set_title_bar(nsh_core::desktop::TitleBarStyle::Hidden, Some(px(44.0)));
    let context = bridge::context();
    native::register_main_window(&context.origin);
    let main_ref = element_ref();
    let shell = bridge::Shell::new(main_ref);
    let script = include_str!("bridge.js")
        .replace("/*__MUTEKI_BROWSER_PAGE__*/", include_str!("browser_page.js"))
        .replace("__MUTEKI_BRIDGE_KEY__", &context.key)
        .replace("__MUTEKI_WINDOW_ID__", "main");
    let main = {
        let (receive, loading, loaded) = (shell.clone(), shell.clone(), shell.clone());
        web_view(format!("{}/chat", context.origin.trim_end_matches('/')))
            .injected_java_script_before_content(script)
            .element_ref(&main_ref)
            .test_id(native::MAIN_ID)
            .on_message(move |data: &str| receive.receive(data))
            .on_load_start(move |_: &WebNavigation| loading.main_loading())
            .on_load(move |_: &WebNavigation| loaded.main_loaded())
            .size_full()
    };
    view().size_full().child(main)
}

const HELP: &str = "Muteki NSH desktop

Usage: muteki-desktop

Starts or reuses the Rust backend and original Next frontend.
MUTEKI_STATE_DIR overrides persistent storage.
MUTEKI_UI_ORIGIN overrides the localhost frontend origin (default http://127.0.0.1:3101).
MUTEKI_UPDATE_TEST_FEED=http://127.0.0.1:PORT replaces GitHub Releases with a local test feed.
Development requires a built apps/frontend/.next and Node on PATH.
The packaged application includes its own Node runtime and frontend.

Storage preparation (without opening a window):\n  --muteki-prepare-storage\n\nUpdate modes (started by the app itself):
  --muteki-channel=candidate --muteki-environment-root=DIR --muteki-self-check
  --muteki-environment-root=DIR --muteki-update-journal=FILE
  --muteki-update-runner=FILE [--recover]";

#[derive(Default)]
struct Options {
    candidate: bool,
    prepare_storage: bool,
    root: Option<std::path::PathBuf>,
    self_check: bool,
    journal: Option<std::path::PathBuf>,
    runner: Option<std::path::PathBuf>,
    recover: bool,
    deep_links: Vec<String>,
}

fn parse() -> Result<Option<Options>, String> {
    let mut options = Options::default();
    for arg in std::env::args().skip(1) {
        let value = |prefix: &str| arg.strip_prefix(prefix).filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--muteki-channel=candidate" => options.candidate = true,
            "--muteki-self-check" => options.self_check = true,
            "--recover" => options.recover = true,
            "--muteki-prepare-storage" => options.prepare_storage = true,
            _ if arg.starts_with("--muteki-environment-root=") => options.root = value("--muteki-environment-root="),
            _ if arg.starts_with("--muteki-update-journal=") => options.journal = value("--muteki-update-journal="),
            _ if arg.starts_with("--muteki-update-runner=") => options.runner = value("--muteki-update-runner="),
            _ if arg.starts_with("muteki://") => options.deep_links.push(arg),
            _ => return Err(format!("Unknown argument: {arg}. Use --help.")),
        }
    }
    let absolute = |path: &Option<std::path::PathBuf>| path.as_ref().is_none_or(|p| p.is_absolute());
    if !absolute(&options.root) || !absolute(&options.journal) || !absolute(&options.runner) {
        return Err("Update paths must be absolute.".into());
    }
    if options.self_check && !(options.candidate && options.root.is_some()) {
        return Err("Candidate self-check requires an isolated candidate environment.".into());
    }
    if options.journal.is_some() && (options.root.is_none() || options.candidate || options.self_check) {
        return Err("An update journal needs the environment root and the stable channel.".into());
    }
    if options.recover && options.runner.is_none() {
        return Err("--recover belongs to the update runner.".into());
    }
    Ok(Some(options))
}

fn main() -> ExitCode {
    match parse() {
        Ok(Some(options)) => run(options),
        Ok(None) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
fn fail_journal(file: &Path, error: &str) {
    if let Err(write) = update_runner::patch(file, serde_json::json!({"phase": "failed", "error": error})) {
        eprintln!("Cannot record the failed update: {write}");
    }
}

/// GUI apps on macOS often inherit a 256 soft (and sometimes hard) NOFILE
/// limit; native engines need more descriptors to spawn. Raise hard first when
/// needed, then soft, without exceeding the requested floor.
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

#[cfg(target_os = "macos")]
fn run(options: Options) -> ExitCode {
    use nsh_platform_macos as platform;
    #[cfg(unix)]
    raise_nofile_soft_limit(65_536);
    if let Some(file) = &options.runner {
        return match update_runner::run_helper(file, options.recover) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Update helper failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if options.prepare_storage {
        let result = storage::installed_root().and_then(|root| {
            let executable = std::env::current_exe().map_err(|e| e.to_string())?;
            storage::prepare_legacy(&root, &executable.with_file_name("muteki-server"))?;
            println!("{}", root.display());
            Ok(())
        });
        return match result { Ok(()) => ExitCode::SUCCESS, Err(error) => { eprintln!("Storage preparation failed: {error}"); ExitCode::FAILURE } };
    }
    if platform::objc2::MainThreadMarker::new().is_none() {
        eprintln!("Muteki must start on the main thread.");
        return ExitCode::FAILURE;
    }
    if let Some(root) = &options.root {
        // SAFETY: still single-threaded; nothing else reads the environment yet.
        unsafe { std::env::set_var("MUTEKI_STATE_DIR", root) };
    }
    if options.self_check {
        let ports = backend::free_port().and_then(|backend| Ok((backend, backend::free_port()?)));
        let Ok((backend_port, ui_port)) = ports else {
            eprintln!("Cannot reserve ports for the candidate check.");
            return ExitCode::FAILURE;
        };
        // SAFETY: still single-threaded.
        unsafe {
            std::env::set_var("MUTEKI_BACKEND_PORT", backend_port.to_string());
            std::env::set_var("MUTEKI_UI_ORIGIN", format!("http://127.0.0.1:{ui_port}"));
        }
        backend::isolate(false);
    }
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("Cannot locate Muteki: {error}");
            return ExitCode::FAILURE;
        }
    };
    let bundled = executable
        .parent()
        .and_then(Path::parent)
        .is_some_and(|directory| directory.file_name().is_some_and(|name| name == "Contents"));
    let root = match backend::state_directory(&executable, bundled) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("Cannot start Muteki: {error}");
            return ExitCode::FAILURE;
        }
    };
    let installed = bundled && !backend::workspace_package(&executable);
    let _storage_lock = if installed {
        match std::fs::create_dir_all(&root).map_err(|error| error.to_string()).and_then(|()| storage::lock(&root.join("desktop.lock"))) {
            Ok(lock) => Some(lock),
            Err(error) => { eprintln!("Cannot open Muteki data: {error}"); if !options.self_check { native::alert(true, "无法打开 Muteki 数据", &error, &["好"]); } return ExitCode::FAILURE; }
        }
    } else { None };
    if installed && std::env::var_os("MUTEKI_BACKEND_PORT").is_none() {
        let (backend_port, ui_port) = match storage::service_ports(&root) {
            Ok(ports) => ports,
            Err(error) => { native::alert(true, "无法读取本机服务配置", &error, &["好"]); return ExitCode::FAILURE; }
        };
        // SAFETY: service threads have not started yet.
        unsafe {
            std::env::set_var("MUTEKI_BACKEND_PORT", backend_port.to_string());
            if std::env::var_os("MUTEKI_UI_ORIGIN").is_none() { std::env::set_var("MUTEKI_UI_ORIGIN", format!("http://127.0.0.1:{ui_port}")); }
        }
        backend::isolate(false);
    }
    if let Some(file) = &options.journal {
        let journal = update_files::read_json(file).ok();
        let valid = file.parent().and_then(Path::parent) == Some(root.join("updates").as_path())
            && journal.as_ref().is_some_and(|j| j["environmentRoot"].as_str() == Some(&*root.to_string_lossy()) && j["phase"] == "validating");
        if !valid {
            eprintln!("Update recovery context does not match this environment.");
            return ExitCode::FAILURE;
        }
        backend::isolate(true);
    }
    let channel = if options.candidate {
        "candidate"
    } else if !bundled || backend::workspace_package(&executable) {
        "development"
    } else {
        "stable"
    };
    updates::configure(updates::Setup { bundle: bundled.then(|| update_files::bundle_of(&executable)).flatten(), channel, root: root.clone() });
    if channel == "stable" && options.journal.is_none() {
        match update_runner::recover_at_launch(&root) {
            Ok(false) => {}
            Ok(true) => return ExitCode::SUCCESS,
            Err(error) => {
                backend::record_startup_error(&executable, bundled, &error);
                eprintln!("Cannot start Muteki: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    let services = match backend::start(&executable, bundled) {
        Ok(services) => services,
        Err(error) => {
            backend::record_startup_error(&executable, bundled, &error);
            eprintln!("Cannot start Muteki: {error}");
            if let Some(file) = &options.journal {
                fail_journal(file, &error);
            }
            if options.self_check {
                let _ = update_files::atomic_json(&root.join("candidate-result.json"), &serde_json::json!({"status": "failed", "error": error}));
            }
            if bundled && !options.self_check { native::alert(true, "Muteki 启动失败", &error, &["好"]); }
            return ExitCode::FAILURE;
        }
    };
    if options.self_check {
        let result = update_runner::self_check();
        drop(services);
        let record = result.clone().unwrap_or_else(|error| serde_json::json!({"status": "failed", "error": error}));
        if let Err(error) = update_files::atomic_json(&root.join("candidate-result.json"), &record) {
            eprintln!("Cannot record the candidate check: {error}");
            return ExitCode::FAILURE;
        }
        return if result.is_ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    if let Some(file) = &options.journal {
        if let Err(error) = update_runner::admit(file) {
            backend::record_startup_error(&executable, bundled, &error);
            fail_journal(file, &error);
            return ExitCode::FAILURE;
        }
        updates::complete_when_ready(file.clone());
    }
    let _services = services;
    for link in options.deep_links {
        bridge::queue_startup_deep_link(link);
    }
    let context = backend::ui_origin()
        .map(|origin| bridge::Context { origin, state: root.clone(), key: uuid::Uuid::new_v4().simple().to_string() });
    match context {
        Ok(context) => {if let Err(error)=bridge::set_context(context){backend::record_startup_error(&executable,bundled,&error.message);eprintln!("Cannot initialize desktop connection: {}",error.message);return ExitCode::FAILURE;}},
        Err(error) => {
            backend::record_startup_error(&executable, bundled, &error);
            eprintln!("Cannot start Muteki: {error}");
            return ExitCode::FAILURE;
        }
    }
    platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<MutekiApp>().app_id(update_files::BUNDLE_ID)),
        platform::AppOptions {
            title: APP_NAME.into(),
            app_id: update_files::BUNDLE_ID.into(),
            size: (1280, 860),
            ..Default::default()
        },
    )
}

#[cfg(not(target_os = "macos"))]
fn run(_: Options) -> ExitCode {
    eprintln!("Muteki desktop requires macOS. Use node scripts/dev.mjs --web for the web UI.");
    ExitCode::FAILURE
}
