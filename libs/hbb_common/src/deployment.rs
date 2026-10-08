//! Deployment policy for the Windows/Android custom client.
//! These are public connection settings, never server private keys or passwords.

pub const ID_SERVER: &str = "remote.yingluozhiwei.cn";
pub const RELAY_SERVER: &str = ID_SERVER;
pub const API_SERVER: &str = "https://remote.yingluozhiwei.cn";
pub const SERVER_PUBLIC_KEY: &str = "CeBYxi7LrLpv+8ESGOrFl3Ad7rds2smfgrOyYJlTTYE=";

pub const OPTIONS: &[(&str, &str)] = &[
    ("custom-rendezvous-server", ID_SERVER),
    ("relay-server", RELAY_SERVER),
    ("api-server", API_SERVER),
    ("key", SERVER_PUBLIC_KEY),
    ("allow-websocket", "Y"),
    ("allow-insecure-tls-fallback", "N"),
    ("force-always-relay", "Y"),
    ("direct-server", "N"),
    ("enable-lan-discovery", "N"),
];

pub fn option(key: &str) -> Option<&'static str> {
    OPTIONS.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

pub fn validate_endpoint(endpoint: &str) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    if endpoint.contains('#') {
        anyhow::bail!("WSS endpoints must not contain a fragment");
    }
    let request = endpoint.into_client_request()?;
    let endpoint = request.uri();
    let trusted_authority = endpoint.authority().map_or(false, |authority| {
        let authority = authority.as_str();
        authority.eq_ignore_ascii_case(ID_SERVER)
            || authority.eq_ignore_ascii_case(RELAY_SERVER)
            || authority.eq_ignore_ascii_case(&format!("{}:443", ID_SERVER))
            || authority.eq_ignore_ascii_case(&format!("{}:443", RELAY_SERVER))
    });
    if endpoint.scheme_str() != Some("wss")
        || !trusted_authority
        || !matches!(endpoint.path(), "/ws/id" | "/ws/relay")
        || endpoint.query().is_some()
    {
        anyhow::bail!("This custom client only connects to its configured WSS server on port 443");
    }
    Ok(())
}
