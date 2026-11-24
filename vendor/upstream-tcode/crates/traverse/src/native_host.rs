//! Native implementation of the transport-agnostic client host contract,
//! shared by the desktop, iOS and Android bootstraps.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use tcode_client::host::{ClientHost, ClientPreferences, HostFuture, PickedImage, Transport};
use tcode_client::pairing::{PairInvite, PairedHost};

use crate::{
    identity::DeviceIdentity,
    lan::{Browse, LanOptions, SystemBrowser},
};

type QrScanner = dyn Fn() -> HostFuture<'static, Result<String, String>>;
type ImagePicker = dyn Fn(usize) -> HostFuture<'static, Result<Vec<PickedImage>, String>>;
type EditorOpener = dyn Fn(&Path) -> Result<(), String>;
type MulticastLock = dyn Fn(bool) + Send + Sync;

/// Native clients share hosts.json, mobile.json, device.json, pairing, and
/// transport policy.
pub struct NativeClientHost {
    speech: Option<std::rc::Rc<dyn tcode_client::speech::SpeechRecognizer>>,
    words: Option<std::rc::Rc<dyn tcode_client::words::WordSegmenter>>,
    data_dir: PathBuf,
    default_device_name: String,
    platform: Option<String>,
    qr_scanner: Option<Box<QrScanner>>,
    image_picker: Option<Box<ImagePicker>>,
    editor: Option<Box<EditorOpener>>,
    multicast_lock: Option<Arc<MulticastLock>>,
    system_browser: Option<Arc<SystemBrowser>>,
    device: OnceLock<Result<DeviceIdentity, String>>,
}

impl NativeClientHost {
    /// The platform starts as this operating system's description; mobile
    /// bootstraps replace it with [`NativeClientHost::with_platform`].
    pub fn new(data_dir: PathBuf, device_name: impl Into<String>) -> Self {
        Self {
            speech: None,
            words: None,
            data_dir,
            default_device_name: device_name.into(),
            platform: default_device_platform(),
            qr_scanner: None,
            image_picker: None,
            editor: None,
            multicast_lock: None,
            system_browser: None,
            device: OnceLock::new(),
        }
    }

    /// The device's Traverse identity, carrying the current name and platform.
    fn device(&self) -> Result<DeviceIdentity, String> {
        let device = self
            .device
            .get_or_init(|| {
                let device = DeviceIdentity::load_or_create(&self.data_dir).map_err(|error| {
                    format!("could not open {}: {error}", crate::identity::DEVICE_FILE)
                })?;
                device.set_lan_options(LanOptions {
                    browse: match &self.system_browser {
                        Some(browser) => Browse::System(browser.clone()),
                        None => Browse::DnsSd,
                    },
                    multicast_lock: self.multicast_lock.clone(),
                });
                Ok(device)
            })
            .clone()?;
        device.set_details(self.device_name(), self.device_platform());
        Ok(device)
    }

    /// The platform saw connectivity change or the app return to the
    /// foreground: rebind the device endpoint's paths and probe every live
    /// transport now. Before the first connection there is nothing to
    /// notify, so the identity is not loaded for this.
    pub fn network_changed(&self) {
        if let Some(Ok(device)) = self.device.get() {
            device.network_changed();
        }
    }

    /// The saved machines changed: the device endpoint, if it is up, follows
    /// their Traverse instances.
    fn hosts_changed(&self) {
        if let Some(Ok(device)) = self.device.get() {
            device.hosts_changed();
        }
    }

    /// Operating system name and version reported to hosts, e.g. `Android 15`.
    pub fn with_platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = Some(platform.into());
        self
    }

    pub fn with_qr_scanner(
        mut self,
        scanner: impl Fn() -> HostFuture<'static, Result<String, String>> + 'static,
    ) -> Self {
        self.qr_scanner = Some(Box::new(scanner));
        self
    }

    /// Segment words with `segmenter` instead of what the client finds on
    /// its own.
    pub fn with_word_segmenter(
        mut self,
        segmenter: impl tcode_client::words::WordSegmenter + 'static,
    ) -> Self {
        self.words = Some(std::rc::Rc::new(segmenter));
        self
    }

    pub fn with_speech_recognizer(
        mut self,
        recognizer: impl tcode_client::speech::SpeechRecognizer + 'static,
    ) -> Self {
        self.speech = Some(std::rc::Rc::new(recognizer));
        self
    }

    /// The platform's own media picker; see [`ClientHost::pick_images`].
    pub fn with_image_picker(
        mut self,
        picker: impl Fn(usize) -> HostFuture<'static, Result<Vec<PickedImage>, String>> + 'static,
    ) -> Self {
        self.image_picker = Some(Box::new(picker));
        self
    }

    /// Held around every DNS-SD browse: Android delivers multicast to an
    /// app only while it holds the Wi-Fi multicast lock. Called from the
    /// transport's threads.
    pub fn with_multicast_lock(mut self, lock: impl Fn(bool) + Send + Sync + 'static) -> Self {
        self.multicast_lock = Some(Arc::new(lock));
        self
    }

    /// Browse DNS-SD through the platform instead of a socket of our own;
    /// iOS reserves raw multicast for entitled apps.
    pub fn with_system_browser(mut self, browser: Arc<SystemBrowser>) -> Self {
        self.system_browser = Some(browser);
        self
    }

    /// Supply the external-editor launcher. It is injected because launching a
    /// process belongs to the composition root that already owns the process
    /// helpers, not to this transport crate.
    pub fn with_editor_opener(
        mut self,
        open: impl Fn(&Path) -> Result<(), String> + 'static,
    ) -> Self {
        self.editor = Some(Box::new(open));
        self
    }

    fn prefs_path(&self) -> PathBuf {
        self.data_dir.join("mobile.json")
    }

    fn prefs(&self) -> serde_json::Value {
        fs::read(self.prefs_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}))
    }

    fn write_prefs(&self, prefs: &serde_json::Value) {
        let result = serde_json::to_vec_pretty(prefs)
            .map_err(io::Error::other)
            .and_then(|bytes| write_private(&self.data_dir, "mobile.json", &bytes));
        if let Err(error) = result {
            log::error!("could not write mobile.json: {error}");
        }
    }
}

impl ClientHost for NativeClientHost {
    fn speech_recognizer(&self) -> Option<std::rc::Rc<dyn tcode_client::speech::SpeechRecognizer>> {
        self.speech.clone()
    }

    fn word_segmenter(&self) -> Option<std::rc::Rc<dyn tcode_client::words::WordSegmenter>> {
        self.words.clone().or_else(tcode_client::words::system)
    }

    fn device_name(&self) -> String {
        self.load_preferences()
            .device_name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| self.default_device_name.clone())
    }

    /// The device's `EndpointId`; the machine authenticates it on every
    /// connection.
    fn device_id(&self) -> String {
        self.device()
            .map(|device| device.endpoint_id().to_string())
            .unwrap_or_else(|error| {
                log::error!("{error}");
                String::new()
            })
    }

    fn device_platform(&self) -> Option<String> {
        self.platform.clone()
    }

    fn load_preferences(&self) -> ClientPreferences {
        let prefs = self.prefs();
        let value = |key: &str| prefs.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        ClientPreferences {
            appearance: value("appearance"),
            language: value("language"),
            device_name: value("device_name"),
            navigation: prefs
                .get("navigation")
                .filter(|value| !value.is_null())
                .cloned(),
            remote_attachment_limit_mib: prefs
                .get("remote_attachment_limit_mib")
                .and_then(serde_json::Value::as_u64)
                .and_then(|limit| u32::try_from(limit).ok()),
        }
    }

    fn save_preferences(&self, preferences: &ClientPreferences) {
        let mut prefs = self.prefs();
        prefs["appearance"] = serde_json::json!(preferences.appearance);
        prefs["language"] = serde_json::json!(preferences.language);
        prefs["device_name"] = serde_json::json!(preferences.device_name);
        prefs["navigation"] = serde_json::json!(preferences.navigation);
        prefs["remote_attachment_limit_mib"] =
            serde_json::json!(preferences.remote_attachment_limit_mib);
        self.write_prefs(&prefs);
    }

    fn outbox_storage(&self, host_id: &str) -> Option<Arc<dyn tcode_client::outbox::Storage>> {
        // Host IDs are opaque; encoding their bytes prevents path traversal.
        let name: String = host_id
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Some(Arc::new(FileOutbox {
            data_dir: self.data_dir.clone(),
            name: format!("outbox-{name}.json"),
        }))
    }

    fn load_hosts(&self) -> Vec<PairedHost> {
        crate::hosts::load_hosts(&self.data_dir).unwrap_or_else(|error| {
            log::error!("could not read hosts.json: {error}");
            Vec::new()
        })
    }

    fn save_hosts(&self, hosts: &[PairedHost]) {
        if let Err(error) = crate::hosts::save_hosts(&self.data_dir, hosts) {
            log::error!("could not write hosts.json: {error}");
        }
        self.hosts_changed();
    }

    fn remember_host(&self, host: PairedHost) {
        if let Err(error) = crate::hosts::update_hosts(&self.data_dir, |hosts| {
            tcode_client::pairing::remember_host(hosts, host);
        }) {
            log::error!("could not save paired machine: {error}");
        }
        self.hosts_changed();
    }

    fn remove_host(&self, host_id: &str) {
        if let Err(error) = crate::hosts::update_hosts(&self.data_dir, |hosts| {
            hosts.retain(|host| host.host_id != host_id);
        }) {
            log::error!("could not remove paired machine: {error}");
        }
        self.hosts_changed();
    }

    fn stamp_connected(&self, host_id: &str, timestamp: u64) {
        if let Err(error) = crate::hosts::update_hosts(&self.data_dir, |hosts| {
            if let Some(host) = hosts.iter_mut().find(|host| host.host_id == host_id) {
                host.last_connected_unix = Some(timestamp);
            }
        }) {
            log::error!("could not record machine connection: {error}");
        }
    }

    fn last_host_id(&self) -> Option<String> {
        self.prefs()
            .get("last_host_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    }

    fn set_last_host_id(&self, host_id: Option<&str>) {
        let mut prefs = self.prefs();
        prefs["last_host_id"] = match host_id {
            Some(id) => serde_json::Value::String(id.to_owned()),
            None => serde_json::Value::Null,
        };
        self.write_prefs(&prefs);
    }

    /// Pairing runs on the Traverse runtime; the UI executor only awaits the
    /// result.
    fn pair(
        &self,
        invite: PairInvite,
        address: Option<std::net::IpAddr>,
    ) -> HostFuture<'_, Result<PairedHost, String>> {
        let device = self.device();
        Box::pin(async move {
            let device = device?;
            let (done, result) = async_channel::bounded(1);
            crate::runtime().spawn(async move {
                let paired = crate::pair(&invite, address, &device)
                    .await
                    .map_err(|error| error.to_string());
                let _ = done.send(paired).await;
            });
            result
                .recv()
                .await
                .unwrap_or_else(|_| Err("pairing was interrupted".into()))
        })
    }

    fn open_in_editor(&self, path: &Path) -> Option<Result<(), String>> {
        self.editor.as_ref().map(|open| open(path))
    }

    fn connect(&self, host: &PairedHost) -> Transport {
        match self.device() {
            Ok(device) => crate::connect(host, &device),
            Err(error) => {
                // A link that reports itself offline instead of a panic in
                // the shell; the profile directory is the thing to fix.
                log::error!("{error}");
                let (to_host, _) = async_channel::unbounded();
                let (_, from_host) = async_channel::unbounded();
                let (state_tx, state) = async_channel::unbounded();
                let _ = state_tx.try_send(tcode_client::ConnectionState::Offline {
                    reason: tcode_client::ConnectionFailure::Unreachable(Some(error.to_string())),
                });
                Transport {
                    originals: None,
                    to_host: to_host.into(),
                    from_host,
                    state,
                    current_host: None,
                }
            }
        }
    }

    fn supports_qr(&self) -> bool {
        self.qr_scanner.is_some()
    }

    fn scan_qr(&self) -> HostFuture<'_, Result<String, String>> {
        self.qr_scanner.as_ref().map_or_else(
            || Box::pin(async { Err("unsupported".into()) }) as HostFuture<'_, _>,
            |scanner| scanner(),
        )
    }

    fn supports_image_picker(&self) -> bool {
        self.image_picker.is_some()
    }

    fn pick_images(&self, limit: usize) -> HostFuture<'_, Result<Vec<PickedImage>, String>> {
        self.image_picker.as_ref().map_or_else(
            || Box::pin(async { Err("unsupported".into()) }) as HostFuture<'_, _>,
            |picker| picker(limit),
        )
    }
}

/// This machine's name, shared by desktop, headless, and native-client defaults.
pub fn default_device_name() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(resolve_default_device_name).clone()
}

fn resolve_default_device_name() -> String {
    resolve_device_name(
        ["HOSTNAME", "HOST", "COMPUTERNAME"]
            .iter()
            .filter_map(|key| std::env::var(key).ok())
            .find_map(|name| clean_name(Some(name))),
        macos_computer_name(),
        system_host_name(),
        fs::read_to_string("/etc/hostname").ok(),
    )
}

fn resolve_device_name(
    env_name: Option<String>,
    computer_name: Option<String>,
    host_name: Option<String>,
    etc_hostname: Option<String>,
) -> String {
    clean_name(env_name)
        .or_else(|| clean_name(computer_name))
        .or_else(|| {
            clean_name(host_name).and_then(|name| clean_name(Some(strip_local_suffix(name))))
        })
        .or_else(|| clean_name(etc_hostname))
        .unwrap_or_else(|| "Tcode".into())
}

/// This operating system's name and version, e.g. `macOS 26.0`,
/// `Windows 10.0.26100` or an `/etc/os-release` pretty name. `None` where the
/// platform bootstrap supplies it instead.
pub fn default_device_platform() -> Option<String> {
    static PLATFORM: OnceLock<Option<String>> = OnceLock::new();
    PLATFORM
        .get_or_init(resolve_default_device_platform)
        .clone()
}

#[cfg(target_os = "macos")]
fn resolve_default_device_platform() -> Option<String> {
    Some(os_with_version("macOS", macos_product_version()))
}

#[cfg(windows)]
fn resolve_default_device_platform() -> Option<String> {
    Some(os_with_version("Windows", windows_version()))
}

#[cfg(target_os = "linux")]
fn resolve_default_device_platform() -> Option<String> {
    Some(
        fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|contents| os_release_pretty_name(&contents))
            .unwrap_or_else(|| "Linux".into()),
    )
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn resolve_default_device_platform() -> Option<String> {
    None
}

#[cfg(any(target_os = "macos", windows))]
fn os_with_version(os: &str, version: Option<String>) -> String {
    match clean_name(version) {
        Some(version) => format!("{os} {version}"),
        None => os.to_owned(),
    }
}

#[cfg(any(target_os = "linux", test))]
fn os_release_pretty_name(contents: &str) -> Option<String> {
    contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("PRETTY_NAME="))
        .and_then(|value| clean_name(Some(value.trim().trim_matches('"').to_owned())))
}

#[cfg(target_os = "macos")]
fn macos_product_version() -> Option<String> {
    let name = c"kern.osproductversion";
    let mut length = 0_usize;
    // SAFETY: a null buffer asks sysctl for the value's length only.
    let probed = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if probed != 0 || length == 0 {
        return None;
    }
    let mut bytes = vec![0_u8; length];
    // SAFETY: `bytes` holds `length` writable bytes and `length` is passed in
    // and out by pointer, so sysctl never writes past the buffer.
    let read = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    bytes.truncate(length.min(bytes.len()));
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8(bytes[..end].to_vec()).ok()
}

#[cfg(windows)]
fn windows_version() -> Option<String> {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `info` is a correctly sized OSVERSIONINFOW that outlives the
    // call; RtlGetVersion only fills its fields. Unlike GetVersionEx it is not
    // capped by the application manifest's compatibility declarations.
    if unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut info) }.is_err() {
        return None;
    }
    Some(format!(
        "{}.{}.{}",
        info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
    ))
}

fn clean_name(name: Option<String>) -> Option<String> {
    name.map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

fn strip_local_suffix(name: String) -> String {
    name.strip_suffix(".local").unwrap_or(&name).to_owned()
}

#[cfg(target_os = "macos")]
fn macos_computer_name() -> Option<String> {
    use core_foundation::{base::TCFType as _, string::CFString};
    use system_configuration_sys::dynamic_store_copy_specific::SCDynamicStoreCopyComputerName;

    let mut encoding = 0;
    // SAFETY: a null store requests the current system value. The returned
    // CFString follows the Create Rule and is transferred to the wrapper.
    let name = unsafe { SCDynamicStoreCopyComputerName(std::ptr::null_mut(), &mut encoding) };
    (!name.is_null()).then(|| unsafe { CFString::wrap_under_create_rule(name) }.to_string())
}

#[cfg(not(target_os = "macos"))]
fn macos_computer_name() -> Option<String> {
    None
}

#[cfg(unix)]
fn system_host_name() -> Option<String> {
    let mut bytes = [0_u8; 256];
    // SAFETY: `bytes` is a valid writable buffer and its exact length is
    // supplied to gethostname. It starts zeroed so the terminator is findable.
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return None;
    }
    let length = bytes.iter().position(|byte| *byte == 0)?;
    String::from_utf8(bytes[..length].to_vec()).ok()
}

#[cfg(not(unix))]
fn system_host_name() -> Option<String> {
    None
}

fn write_private(data_dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = data_dir.join(name);
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    use io::Write as _;
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

struct FileOutbox {
    data_dir: PathBuf,
    name: String,
}

impl tcode_client::outbox::Storage for FileOutbox {
    fn load(&self) -> Result<Vec<tcode_client::outbox::Entry>, tcode_protocol::ProtocolError> {
        match fs::read(self.data_dir.join(&self.name)) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(tcode_client::outbox::storage_error)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(tcode_client::outbox::storage_error(error)),
        }
    }
    fn save(
        &self,
        entries: &[tcode_client::outbox::Entry],
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let bytes = serde_json::to_vec(entries).map_err(tcode_client::outbox::storage_error)?;
        let temporary = format!("{}.tmp", self.name);
        write_private(&self.data_dir, &temporary, &bytes)
            .and_then(|()| {
                fs::rename(
                    self.data_dir.join(&temporary),
                    self.data_dir.join(&self.name),
                )
            })
            .map_err(tcode_client::outbox::storage_error)
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    const MACHINE_ID: &str = "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf";

    #[test]
    fn connection_stamp_preserves_an_address_update_already_in_progress() {
        let dir = TestDir::new();
        let client = NativeClientHost::new(dir.0.clone(), "phone");
        client.remember_host(PairedHost {
            space_id: None,
            space_name: None,
            host_id: MACHINE_ID.into(),
            name: "Machine".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["192.168.31.5:47420".into()],
            last_connected_unix: None,
            group_id: None,
        });
        let (locked, received) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let transport_dir = dir.0.clone();
        let transport = std::thread::spawn(move || {
            crate::hosts::update_hosts(&transport_dir, |hosts| {
                hosts[0].addrs.insert(0, "192.168.1.161:47420".into());
                locked.send(()).unwrap();
                released.recv().unwrap();
            })
            .unwrap();
        });
        received.recv().unwrap();
        let (started, starting) = std::sync::mpsc::channel();
        let stamp_dir = dir.0.clone();
        let stamp = std::thread::spawn(move || {
            let client = NativeClientHost::new(stamp_dir, "phone");
            started.send(()).unwrap();
            client.stamp_connected(MACHINE_ID, 1234);
        });
        starting.recv().unwrap();
        release.send(()).unwrap();
        transport.join().unwrap();
        stamp.join().unwrap();
        let saved = client.load_hosts().remove(0);
        assert_eq!(
            saved.addrs,
            vec!["192.168.1.161:47420", "192.168.31.5:47420"]
        );
        assert_eq!(saved.last_connected_unix, Some(1234));
    }

    #[test]
    fn device_name_uses_the_first_nonempty_source_and_strips_only_local_host_suffix() {
        for (sources, expected) in [
            (
                [
                    Some(" Explicit name "),
                    Some("Friendly Mac"),
                    Some("host.local"),
                    Some("etc-host"),
                ],
                "Explicit name",
            ),
            (
                [Some(" "), Some("Friendly Mac"), Some("host.local"), None],
                "Friendly Mac",
            ),
            (
                [None, Some(""), Some("studio.local"), Some("etc-host")],
                "studio",
            ),
            (
                [None, None, Some("studio.localdomain"), None],
                "studio.localdomain",
            ),
            ([None, None, Some(".local"), Some(" etc-host ")], "etc-host"),
            ([None, None, None, None], "Tcode"),
        ] {
            let [environment, computer, host, etc] = sources.map(|value| value.map(str::to_owned));
            assert_eq!(
                resolve_device_name(environment, computer, host, etc),
                expected,
                "{sources:?}"
            );
        }
    }

    #[test]
    fn os_release_pretty_name_is_unquoted() {
        for (contents, expected) in [
            (
                "NAME=Ubuntu\nVERSION_ID=24.04\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n",
                Some("Ubuntu 24.04.1 LTS"),
            ),
            ("PRETTY_NAME=Alpine Linux\n", Some("Alpine Linux")),
            (" PRETTY_NAME=\"  Fedora Linux  \" \n", Some("Fedora Linux")),
            ("ID=alpine\nPRETTY_NAME=\"\"\n", None),
            ("NAME=Alpine\n", None),
            ("", None),
        ] {
            assert_eq!(
                os_release_pretty_name(contents).as_deref(),
                expected,
                "{contents:?}"
            );
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "tcode-native-client-host-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn mobile_preferences_preserve_device_identity_last_host_and_unknown_fields() {
        let dir = TestDir::new();
        fs::write(
            dir.0.join("mobile.json"),
            br#"{
                "appearance": "dark",
                "language": "zh-CN",
                "device_name": "My phone",
                "last_host_id": "host-before-client-seam",
                "future_field": {"preserve": true}
            }"#,
        )
        .unwrap();
        let host = NativeClientHost::new(dir.0.clone(), "fallback").with_platform("Android 15");

        assert_eq!(
            host.load_preferences(),
            ClientPreferences {
                appearance: Some("dark".into()),
                language: Some("zh-CN".into()),
                device_name: Some("My phone".into()),
                ..Default::default()
            }
        );
        assert_eq!(
            host.last_host_id().as_deref(),
            Some("host-before-client-seam")
        );
        let device_id = host.device_id();
        assert!(tcode_client::host::valid_device_id(&device_id));
        assert_eq!(host.device_id(), device_id);
        let identity: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join("device.json")).unwrap()).unwrap();
        assert_eq!(identity["v"], 1);
        assert_eq!(identity["name"], "My phone");
        assert_eq!(identity["platform"], "Android 15");
        host.save_preferences(&ClientPreferences {
            appearance: Some("light".into()),
            language: None,
            device_name: Some("Renamed".into()),
            navigation: Some(serde_json::json!({"history": ["hosts", "threads"]})),
            remote_attachment_limit_mib: Some(4),
        });

        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join("mobile.json")).unwrap()).unwrap();
        assert_eq!(saved["last_host_id"], "host-before-client-seam");
        assert_eq!(saved["future_field"]["preserve"], true);
        assert_eq!(saved["appearance"], "light");
        assert_eq!(
            host.load_preferences().navigation.unwrap()["history"],
            serde_json::json!(["hosts", "threads"])
        );
        assert!(saved["language"].is_null());
        assert_eq!(saved["remote_attachment_limit_mib"], 4);
        assert_eq!(host.load_preferences().remote_attachment_limit_mib, Some(4));

        host.set_last_host_id(Some("next-host"));
        assert_eq!(host.last_host_id().as_deref(), Some("next-host"));
        assert_eq!(host.device_name(), "Renamed");
        assert_eq!(host.device_id(), device_id);
        // Check the write before reopening can refresh the identity's details.
        let identity: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join("device.json")).unwrap()).unwrap();
        assert_eq!(identity["v"], 1);
        assert_eq!(identity["name"], "Renamed");
        assert_eq!(identity["platform"], "Android 15");
        let loaded = DeviceIdentity::load_or_create(&dir.0).unwrap();
        assert_eq!(loaded.endpoint_id().to_string(), device_id);
        assert_eq!(loaded.claim().name, "Renamed");
        assert_eq!(loaded.claim().platform.as_deref(), Some("Android 15"));
        let reopened = NativeClientHost::new(dir.0.clone(), "fallback").with_platform("Android 15");
        assert_eq!(reopened.device_id(), device_id);
        assert_eq!(reopened.last_host_id().as_deref(), Some("next-host"));
        assert_eq!(reopened.device_name(), "Renamed");
    }
}
