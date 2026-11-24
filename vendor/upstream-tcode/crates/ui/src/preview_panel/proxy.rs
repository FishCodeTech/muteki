//! Attached-browser configuration absent from wry's endpoint type. The bridge
//! asks for no credentials: the paired connection is the authority.
use tcode_traverse::preview::ProxyEntry;

pub(super) fn builder<'a>(
    builder: wry::WebViewBuilder<'a>,
    host: Option<&ProxyEntry>,
) -> Result<wry::WebViewBuilder<'a>, String> {
    let Some(_host) = host else {
        return Ok(builder);
    };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        // Every attached browser gets a separate nonpersistent WK store.
        // Cookies are scoped by hostname, not by our allocated viewing ports.
        Ok(builder.with_incognito(true))
    }
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    let builder = {
        let origin = url::Url::parse(&_host.origin).map_err(|e| e.to_string())?;
        builder
            .with_incognito(true)
            .with_proxy_config(wry::ProxyConfig::Http(wry::ProxyEndpoint {
                host: origin.host_str().ok_or("missing proxy host")?.into(),
                port: origin
                    .port_or_known_default()
                    .ok_or("missing proxy port")?
                    .to_string(),
            }))
    };
    #[cfg(target_os = "windows")]
    {
        use wry::WebViewBuilderExtWindows as _;
        // wry's additional arguments replace its generated proxy arguments.
        Ok(builder.with_additional_browser_args(format!(
            "--proxy-server={} --proxy-bypass-list=<-loopback>",
            _host.origin
        )))
    }
    // lb-wry gives WebKitGTK's ephemeral context a custom proxy with no
    // ignored hosts, so loopback goes through the bridge too.
    #[cfg(target_os = "linux")]
    {
        Ok(builder)
    }
}
