//! Saved machines and pairing invitations shared by all clients.
//!
//! A machine is identified by its iroh `EndpointId`; every other field is a
//! routing hint that discovery services may extend but never replace.
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use url::Url;

/// A pairing is bound to the machine identity (`host_id`), never to an
/// address. The relay and addresses are the ones authenticated connections
/// actually used, refreshed after each so the next launch starts from what
/// worked last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedHost {
    /// The machine's `EndpointId`.
    pub host_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_name: Option<String>,
    /// The Traverse instances the machine publishes to, as its invitation
    /// listed them; see [`PairInvite::traverse`]. Records written while this
    /// was one optional value read as the same list.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "traverse_list"
    )]
    pub traverse: Vec<String>,
    /// The machine's home relay, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// Direct `ip:port` addresses the machine was last reachable at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_unix: Option<u64>,
    /// The group this machine belongs to, as a machine of the group last
    /// told this device; its machines are saved beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
}

/// `traverse` as a list, or as the single optional value older records
/// hold: `null` is the official instance, a string that one entry.
fn traverse_list<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        List(Vec<String>),
        One(Option<String>),
    }
    Ok(match Stored::deserialize(deserializer)? {
        Stored::List(list) => list,
        Stored::One(one) => one.into_iter().collect(),
    })
}

/// Direct addresses kept per machine.
pub const MAX_ADDRS: usize = 16;
/// The `traverse` entry naming the official instance.
pub const TRAVERSE_OFFICIAL: &str = "official";
/// The single `traverse` entry of a machine that publishes to no service,
/// so a device tells it apart from one on the official service and loads no
/// manifest for it.
pub const TRAVERSE_OFF: &str = "off";
/// `traverse` entries a link may carry.
pub const MAX_TRAVERSE: usize = 8;
/// The longest link accepted.
pub const MAX_LINK_LEN: usize = 1024;

/// Record a host in the saved list. A host is identified by `host_id`, so
/// pairing again or stamping a reconnection replaces its record instead of
/// adding a second row; the newest record moves to the end.
pub fn remember_host(hosts: &mut Vec<PairedHost>, host: PairedHost) {
    hosts.retain(|existing| existing.host_id != host.host_id);
    hosts.push(host);
}

/// What a `tcode://pair` link carries; the format's contract is
/// `docs/pair-link.md`. First contact is always by scanning or pasting the
/// link, so the link itself is the secret; there is no separate code. It
/// names no address and no machine name: the name comes from the machine's
/// reply, addresses from lookups and the connection. Space links are
/// reusable. A browser leaves `host_id` empty and pairs with the origin that
/// served it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairInvite {
    pub host_id: String,
    /// [`SECRET_BYTES`] random bytes as unpadded base64url.
    pub secret: String,
    pub space: Option<String>,
    /// A group link names the machine's group; never with `space`.
    pub group: Option<String>,
    /// Every Traverse instance the machine publishes to:
    /// [`TRAVERSE_OFFICIAL`] or an instance's base URL. Empty is the official
    /// instance only; the single entry [`TRAVERSE_OFF`] is none at all.
    pub traverse: Vec<String>,
    pub relay: Option<String>,
    /// The machine's bound UDP port, dialed only at an address the user
    /// types.
    pub port: u16,
}

impl PairInvite {
    /// The saved record a completed pairing produces, named as the machine
    /// introduced itself.
    pub fn paired(&self, host_name: String) -> PairedHost {
        PairedHost {
            host_id: self.host_id.clone(),
            name: host_name,
            space_id: self.space.clone(),
            space_name: None,
            traverse: self.traverse.clone(),
            relay: self.relay.clone(),
            addrs: Vec::new(),
            last_connected_unix: None,
            group_id: None,
        }
    }
}

/// An `EndpointId` as printed by iroh: 64 hex characters.
pub fn valid_host_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Entropy of an invitation secret.
pub const SECRET_BYTES: usize = 16;
/// Length of an encoded secret: 16 bytes as unpadded base64url.
pub const SECRET_LEN: usize = 22;

/// Encode random bytes as the secret a link carries.
pub fn encode_secret(bytes: &[u8; SECRET_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Whether `secret` is the canonical encoding of [`SECRET_BYTES`] bytes.
pub fn valid_invitation_secret(secret: &str) -> bool {
    secret.len() == SECRET_LEN
        && URL_SAFE_NO_PAD
            .decode(secret)
            .is_ok_and(|bytes| bytes.len() == SECRET_BYTES)
}

pub fn valid_space_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && !id.chars().any(char::is_control)
}

fn valid_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|host| !host.is_empty())
    })
}

/// A port as a link writes it: canonical decimal, never zero.
fn parse_port(value: &str) -> Option<u16> {
    let port: u16 = value.parse().ok()?;
    (port != 0 && port.to_string() == value).then_some(port)
}

pub fn parse_pair_url(value: &str) -> Option<PairInvite> {
    let value = value.trim();
    if value.len() > MAX_LINK_LEN {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if url.scheme() != "tcode" || url.host_str() != Some("pair") {
        return None;
    }
    let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let all = |name: &'static str| {
        pairs
            .iter()
            .filter(move |(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    // A single-valued parameter: `Err` when repeated.
    let field = |name: &'static str| -> Result<Option<String>, ()> {
        let mut values = all(name);
        let first = values.next().map(str::to_owned);
        match values.next() {
            Some(_) => Err(()),
            None => Ok(first),
        }
    };
    if field("v").ok()?? != "3" {
        return None;
    }
    let host_id = field("id").ok()??;
    if !valid_host_id(&host_id) {
        return None;
    }
    let secret = field("secret").ok()??;
    if !valid_invitation_secret(&secret) {
        return None;
    }
    let space = field("space").ok()?;
    if space.as_deref().is_some_and(|id| !valid_space_id(id)) {
        return None;
    }
    let group = field("group").ok()?;
    if group
        .as_deref()
        .is_some_and(|id| space.is_some() || !valid_invitation_secret(id))
    {
        return None;
    }
    let entries: Vec<&str> = all("traverse").collect();
    let off_alone = entries == [TRAVERSE_OFF];
    if entries.len() > MAX_TRAVERSE
        || entries.iter().any(|value| {
            !(*value == TRAVERSE_OFFICIAL
                || valid_url(value)
                || (*value == TRAVERSE_OFF && off_alone))
        })
    {
        return None;
    }
    let mut traverse: Vec<String> = Vec::new();
    for value in entries {
        if !traverse.iter().any(|known| known == value) {
            traverse.push(value.to_owned());
        }
    }
    let relay = field("relay").ok()?;
    if relay.as_deref().is_some_and(|url| !valid_url(url)) {
        return None;
    }
    let port = parse_port(&field("port").ok()??)?;
    Some(PairInvite {
        host_id,
        secret,
        space,
        group,
        traverse,
        relay,
        port,
    })
}

pub fn pair_url(invite: &PairInvite) -> String {
    let mut url = Url::parse("tcode://pair").expect("static pairing URL is valid");
    let mut query = url.query_pairs_mut();
    query
        .append_pair("v", "3")
        .append_pair("id", &invite.host_id)
        .append_pair("secret", &invite.secret);
    if let Some(space) = &invite.space {
        query.append_pair("space", space);
    }
    if let Some(group) = &invite.group {
        query.append_pair("group", group);
    }
    for traverse in invite.traverse.iter().take(MAX_TRAVERSE) {
        query.append_pair("traverse", traverse);
    }
    if let Some(relay) = &invite.relay {
        query.append_pair("relay", relay);
    }
    query.append_pair("port", &invite.port.to_string());
    drop(query);
    url.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "a5f3c6ea6ba6c5c5d0c4e2b7a5f3c6ea6ba6c5c5d0c4e2b7a5f3c6ea6ba6c5c5";

    #[test]
    fn pairing_again_replaces_the_saved_host_instead_of_duplicating_it() {
        let host = |id: &str, name: &str| PairedHost {
            host_id: id.into(),
            name: name.into(),
            space_id: None,
            space_name: None,
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["192.168.1.2:47420".into()],
            last_connected_unix: None,
            group_id: None,
        };
        let mut hosts = vec![host("desk", "first"), host("laptop", "laptop")];
        remember_host(&mut hosts, host("desk", "second"));
        assert_eq!(
            hosts,
            vec![host("laptop", "laptop"), host("desk", "second")]
        );
    }

    #[test]
    fn saved_hosts_persist_identity_and_hints_only() {
        let host: PairedHost = serde_json::from_str(&format!(
            r#"{{"host_id":"{ID}","name":"Desk","relay":"https://euw1-1.relay.iroh.network./","addrs":["192.168.1.2:47420"],"last_connected_unix":42}}"#
        ))
        .unwrap();
        assert!(host.traverse.is_empty());
        assert_eq!(
            host.relay.as_deref(),
            Some("https://euw1-1.relay.iroh.network./")
        );
        assert_eq!(
            serde_json::to_value(&host).unwrap(),
            serde_json::json!({"host_id":ID,"name":"Desk","relay":"https://euw1-1.relay.iroh.network./","addrs":["192.168.1.2:47420"],"last_connected_unix":42})
        );
        let minimal: PairedHost =
            serde_json::from_str(&format!(r#"{{"host_id":"{ID}","name":"Desk"}}"#)).unwrap();
        assert!(minimal.addrs.is_empty());
        assert_eq!(minimal.last_connected_unix, None);
    }

    /// `hosts.json` written while `traverse` held one optional value still
    /// loads, meaning what it meant then, and is written back as a list.
    #[test]
    fn saved_hosts_from_the_single_traverse_value_load_as_the_list() {
        let load = |traverse: &str| -> PairedHost {
            serde_json::from_str(&format!(
                r#"{{"host_id":"{ID}","name":"Desk","traverse":{traverse}}}"#
            ))
            .unwrap()
        };
        assert!(load("null").traverse.is_empty());
        assert_eq!(load(r#""off""#).traverse, [TRAVERSE_OFF]);
        let custom = load(r#""https://traverse.example/""#);
        assert_eq!(custom.traverse, ["https://traverse.example/"]);
        assert_eq!(
            serde_json::to_value(&custom).unwrap()["traverse"],
            serde_json::json!(["https://traverse.example/"])
        );
        assert_eq!(
            load(r#"["official","https://traverse.example/"]"#).traverse,
            [TRAVERSE_OFFICIAL, "https://traverse.example/"]
        );
    }

    const SECRET: &str = "AAECAwQFBgcICQoLDA0ODw";

    #[test]
    fn invitations_round_trip_and_reject_malformed_fields() {
        // The example in docs/pair-link.md: official only, no space.
        let official = format!(
            "tcode://pair?v=3&id={ID}&secret={SECRET}&relay=https%3A%2F%2Fuse1-1.relay.n0.iroh.link.%2F&port=47420"
        );
        let invite = parse_pair_url(&official).unwrap();
        assert_eq!(
            invite,
            PairInvite {
                host_id: ID.into(),
                secret: encode_secret(&std::array::from_fn(|i| i as u8)),
                space: None,
                group: None,
                traverse: Vec::new(),
                relay: Some("https://use1-1.relay.n0.iroh.link./".into()),
                port: 47420,
            }
        );
        assert_eq!(pair_url(&invite), official);
        assert!(official.len() < 200, "{}", official.len());

        let wire = format!(
            "tcode://pair?v=3&id={ID}&secret={SECRET}&space=shared&traverse=official&traverse=https%3A%2F%2Ftraverse.example%2F&relay=https%3A%2F%2Frelay.example%2F&port=5000"
        );
        let invite = parse_pair_url(&wire).unwrap();
        assert_eq!(
            invite,
            PairInvite {
                host_id: ID.into(),
                secret: SECRET.into(),
                space: Some("shared".into()),
                group: None,
                traverse: vec![TRAVERSE_OFFICIAL.into(), "https://traverse.example/".into()],
                relay: Some("https://relay.example/".into()),
                port: 5000,
            }
        );
        assert_eq!(pair_url(&invite), wire);
        // Any order parses.
        assert_eq!(
            parse_pair_url(&format!(
                "tcode://pair?port=5000&relay=https%3A%2F%2Frelay.example%2F&traverse=official&space=shared&secret={SECRET}&traverse=https%3A%2F%2Ftraverse.example%2F&id={ID}&v=3"
            )),
            Some(invite)
        );

        let lan_only = format!("tcode://pair?v=3&id={ID}&secret={SECRET}&traverse=off&port=47420");
        let off = parse_pair_url(&lan_only).unwrap();
        assert_eq!(off.traverse, [TRAVERSE_OFF]);
        assert_eq!(off.relay, None);
        assert_eq!(pair_url(&off), lan_only);

        // A group link names the group, never a space as well.
        let group_link = format!(
            "tcode://pair?v=3&id={ID}&secret={SECRET}&group={SECRET}&traverse=off&port=47420"
        );
        let group = parse_pair_url(&group_link).unwrap();
        assert_eq!(group.group.as_deref(), Some(SECRET));
        assert_eq!(pair_url(&group), group_link);
        assert!(parse_pair_url(&group_link.replace("&group=", "&space=shared&group=")).is_none());
        assert!(
            parse_pair_url(&group_link.replace(&format!("group={SECRET}"), "group=home")).is_none()
        );

        let url_safe_secret = "__-_AAAAAAAAAAAAAAAAAA";
        assert_eq!(
            parse_pair_url(&wire.replace(SECRET, url_safe_secret))
                .unwrap()
                .secret,
            url_safe_secret
        );
        for bad in [
            "",
            "123456",
            "AAECAwQFBgcICQoLDA0OD",
            "AAECAwQFBgcICQoLDA0ODw%3D%3D",
            "AAECAwQFBgcICQoLDA0ODx",
            "AAECAwQFBgcICQoLDA0OD%2B",
            "AAECAwQFBgcICQoLDA0ODwAA",
        ] {
            assert!(
                parse_pair_url(&wire.replace(SECRET, bad)).is_none(),
                "{bad:?}"
            );
        }
        for (field, replacement) in [
            ("v=3", "v=2"),
            ("v=3", "v=4"),
            ("v=3&", ""),
            ("tcode://", "https://"),
            ("tcode://pair", "tcode://join"),
            // A link from before the secret carried a six-digit code.
            (&format!("secret={SECRET}"), "code=123456"),
            (&format!("id={ID}"), "id=desk"),
            (&format!("id={ID}&"), ""),
            ("port=5000", "port=0"),
            ("port=5000", "port=65536"),
            ("port=5000", "port=05000"),
            ("port=5000", "port=%2B5000"),
            ("port=5000", "port=desk"),
            ("&port=5000", ""),
            (
                "relay=https%3A%2F%2Frelay.example%2F",
                "relay=ftp%3A%2F%2Frelay",
            ),
            (
                "traverse=https%3A%2F%2Ftraverse.example%2F",
                "traverse=disabled",
            ),
            (
                "traverse=https%3A%2F%2Ftraverse.example%2F",
                "traverse=file%3A%2F%2F%2Ftmp%2Frelays.json",
            ),
            // `off` stands alone.
            ("traverse=official", "traverse=off"),
            // Single-valued parameters appear once.
            ("port=5000", "port=5000&port=5000"),
            ("v=3", "v=3&v=3"),
            ("space=shared", "space=shared&space=other"),
            (
                "relay=https%3A%2F%2Frelay.example%2F",
                "relay=https%3A%2F%2Frelay.example%2F&relay=https%3A%2F%2Frelay.example%2F",
            ),
        ] {
            assert!(
                parse_pair_url(&wire.replace(field, replacement)).is_none(),
                "{field} -> {replacement}"
            );
        }
        // A version 2 link, as the previous release printed it.
        assert!(
            parse_pair_url(&format!(
                "tcode://pair?v=2&id={ID}&secret={SECRET}&name=Desk&addr=192.168.1.2%3A47420"
            ))
            .is_none()
        );
        let traverse_entries = |count: usize| {
            let entries: String = (0..count)
                .map(|n| format!("&traverse=https%3A%2F%2Ft{n}.example%2F"))
                .collect();
            format!("tcode://pair?v=3&id={ID}&secret={SECRET}{entries}&port=1")
        };
        assert_eq!(
            parse_pair_url(&traverse_entries(MAX_TRAVERSE))
                .unwrap()
                .traverse
                .len(),
            MAX_TRAVERSE
        );
        assert!(parse_pair_url(&traverse_entries(MAX_TRAVERSE + 1)).is_none());
        assert!(parse_pair_url(&format!("{wire}&padding={}", "x".repeat(4096))).is_none());
        let at_limit = format!(
            "{official}&padding={}",
            "x".repeat(MAX_LINK_LEN - official.len() - "&padding=".len())
        );
        assert!(parse_pair_url(&at_limit).is_some());
        assert!(parse_pair_url(&format!("{at_limit}x")).is_none());
        for space in ["".to_owned(), "s".repeat(65), "%00".into(), "%0A".into()] {
            assert!(
                parse_pair_url(&wire.replace("space=shared", &format!("space={space}"))).is_none()
            );
        }
    }
}
