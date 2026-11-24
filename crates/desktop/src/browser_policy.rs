//! Operator-configured exceptions for self-development browser access.
//! Origins are exact: a grant for 127.0.0.1 never grants localhost or another port.
pub fn control_origins() -> Vec<String> {
    static ORIGINS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        let configured = std::env::var("MUTEKI_BROWSER_CONTROL_ORIGINS").unwrap_or_default();
        let mut origins = Vec::new();
        for value in configured.split(',').map(str::trim).filter(|value| !value.is_empty()) {
            let parsed = reqwest::Url::parse(value).ok().filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some_and(|host| host.chars().all(|ch| ch.is_ascii_alphanumeric() || ".-[]:".contains(ch)))
                    && url.username().is_empty() && url.password().is_none()
                    && url.path() == "/" && url.query().is_none() && url.fragment().is_none()
            });
            let Some(url) = parsed else {
                eprintln!("Invalid MUTEKI_BROWSER_CONTROL_ORIGINS: expected comma-separated HTTP(S) origins without paths or credentials; no control origins granted");
                return Vec::new();
            };
            origins.push(url.origin().ascii_serialization());
        }
        origins.sort(); origins.dedup(); origins
    });
    ORIGINS.clone()
}

pub fn allowed(url: &reqwest::Url, origins: &[String]) -> bool {
    if !matches!(url.scheme(), "http" | "https") { return false; }
    url.username().is_empty() && url.password().is_none()
        && origins.contains(&url.origin().ascii_serialization())
}

pub fn protected_ports(api_port: u16, ui_origin: Option<&str>) -> Vec<u16> {
    let mut ports = frontend_ports(ui_origin);
    ports.push(api_port);
    ports.sort_unstable();
    ports.dedup();
    ports
}

pub fn browser_origins(api_port: u16, ui_origin: Option<&str>) -> Vec<String> {
    let mut origins = control_origins();
    for port in frontend_ports(ui_origin).into_iter().filter(|port| *port != api_port) {
        let authority_port = if port == 80 { String::new() } else { format!(":{port}") };
        origins.push(format!("http://127.0.0.1{authority_port}"));
        origins.push(format!("http://[::1]{authority_port}"));
        origins.push(format!("http://localhost{authority_port}"));
    }
    if let Some(url) = frontend_url(ui_origin)
        && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))
        && url.port_or_known_default() != Some(api_port)
    {
        origins.push(url.origin().ascii_serialization());
    }
    origins.sort_unstable();
    origins.dedup();
    origins
}

fn frontend_url(ui_origin: Option<&str>) -> Option<reqwest::Url> {
    let configured = std::env::var("MUTEKI_UI_ORIGIN").ok();
    let url = reqwest::Url::parse(ui_origin.or(configured.as_deref())?).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn frontend_ports(ui_origin: Option<&str>) -> Vec<u16> {
    let mut ports = vec![3101, 3103];
    if let Some(port) = std::env::var("MUTEKI_UI_PORT").ok().and_then(|port| port.parse().ok()) {
        ports.push(port);
    }
    if let Some(port) = frontend_url(ui_origin).and_then(|url| url.port_or_known_default()) {
        ports.push(port);
    }
    ports
}
