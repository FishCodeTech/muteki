use std::{
    collections::BTreeMap,
    io::Write,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, SystemTime},
};
use tcode_core::{pull_request::HostKind, settings::HostSettings};
use tcode_services::github::{
    CredentialError, GitHubApi, GitHubError, RequestOptions, RestRequest,
    api::Authentication,
    graphql::{self, AliasItem, Document},
};

#[path = "support/github.rs"]
mod fixture;
use fixture::{Fixture, Store};

fn quota(resource: &str, remaining: u64, reset: u64) -> String {
    format!(
        "x-ratelimit-resource: {resource}\r\nx-ratelimit-limit: 100\r\nx-ratelimit-remaining: {remaining}\r\nx-ratelimit-reset: {reset}\r\n"
    )
}
fn reset() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600
}
fn document() -> Document {
    Document {
        query: "query Fixture { viewer { login } }".into(),
        variables: BTreeMap::new(),
    }
}

#[test]
fn maps_hosts_and_keeps_saved_environment_and_enterprise_credentials_in_their_boundaries() {
    let fixture = Fixture::new();
    let store = Store::new();
    let credentials = store.credentials(&[
        ("GH_TOKEN", "public-first"),
        ("GITHUB_TOKEN", "public-second"),
        ("GH_ENTERPRISE_TOKEN", "enterprise-first"),
        ("GITHUB_ENTERPRISE_TOKEN", "enterprise-second"),
        ("GH_HOST", " GIT.EXAMPLE.COM "),
    ]);
    let api = GitHubApi::new(credentials.clone(), fixture.builder());
    for (host, domain, path, token) in [
        (" GITHUB.COM ", "api.github.com", "/user", "public-first"),
        (
            "tenant.ghe.com",
            "api.tenant.ghe.com",
            "/user",
            "public-first",
        ),
        (
            "git.example.com",
            "git.example.com",
            "/api/v3/user",
            "enterprise-first",
        ),
    ] {
        let (answer, request, _) = fixture.call(
            || api.rest(host, RestRequest::get("/user"), &RequestOptions::default()),
            200,
            "",
            b"{}",
        );
        answer.unwrap();
        assert!(request.starts_with(&format!("GET {path} ")));
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("host: {domain}\r\n"))
        );
        assert!(request.contains(&format!("Bearer {token}")));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("user-agent: tcode\r\n")
        );
    }
    let (answer, request, _) = fixture.call(
        || api.graphql("git.example.com", &document(), &RequestOptions::default()),
        200,
        "",
        b"{\"data\":{}}",
    );
    answer.unwrap();
    assert!(request.starts_with("POST /api/graphql "));
    store
        .store
        .set_token(
            tcode_core::pull_request::HostKind::Github,
            "GITHUB.COM",
            Some("saved-first"),
        )
        .unwrap();
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer saved-first"));
    store
        .store
        .set_token(
            tcode_core::pull_request::HostKind::Github,
            "github.com",
            Some("replacement"),
        )
        .unwrap();
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer replacement"));
    store
        .store
        .set_token(
            tcode_core::pull_request::HostKind::Github,
            "github.com",
            None,
        )
        .unwrap();
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer public-first"));
    credentials.configure(BTreeMap::from([(
        "github.com".into(),
        HostSettings {
            kind: HostKind::Github,
            enabled: false,
            account: None,
        },
    )]));
    assert_eq!(
        api.rest(
            "github.com",
            RestRequest::get("/user"),
            &RequestOptions::default()
        )
        .unwrap_err(),
        GitHubError::Credential(CredentialError::Disabled)
    );
    // Anonymous release checks never read a saved token or the disabled switch.
    let (answer, request, _) = fixture.call(
        || tcode_services::version_check::ReleaseFeed::new(api.clone()).latest_json(),
        200,
        "",
        b"{\"tag_name\":\"v99.0.0\"}",
    );
    answer.unwrap();
    assert!(request.starts_with("GET /repos/Tryanks/tcode/releases/latest "));
    assert!(!request.to_ascii_lowercase().contains("authorization"));
}

#[test]
fn classifies_graphql_partial_errors_rate_limits_and_rest_errors_before_http_success() {
    for (status, headers, body, expected) in [
        (
            200,
            "",
            r#"{"data":{"viewer":{}},"errors":[{"message":"bad"}]}"#,
            "response",
        ),
        (
            200,
            "",
            r#"{"errors":[{"type":"RATE_LIMITED"}]}"#,
            "limited",
        ),
        (
            200,
            "x-ratelimit-remaining: 0\r\n",
            r#"{"errors":[{"message":"quota exhausted"}]}"#,
            "limited",
        ),
        (
            200,
            "",
            r#"{"errors":[{"message":"API rate limit already exceeded"}]}"#,
            "limited",
        ),
        (
            200,
            "",
            r#"{"errors":[{"message":"API rate limit exceeded"}]}"#,
            "limited",
        ),
        (
            401,
            "",
            r#"{"errors":[{"type":"RATE_LIMITED"}]}"#,
            "limited",
        ),
        (200, "", r#"{"errors":[{"type":"NOT_FOUND"}]}"#, "missing"),
        (
            200,
            "",
            r#"{"errors":[{"type":"NOT_FOUND"},{"type":"FORBIDDEN"}]}"#,
            "response",
        ),
    ] {
        let fixture = Fixture::new();
        let store = Store::new();
        let api = GitHubApi::new(
            store.credentials(&[("GH_TOKEN", "fixture")]),
            fixture.builder(),
        );
        let (answer, _, _) = fixture.call(
            || api.graphql("github.com", &document(), &RequestOptions::default()),
            status,
            headers,
            body.as_bytes(),
        );
        match expected {
            "response" => assert!(matches!(answer, Err(GitHubError::Response { .. }))),
            "limited" => {
                assert!(matches!(answer, Err(GitHubError::RateLimited { .. })));
                assert!(matches!(
                    api.rest(
                        "github.com",
                        RestRequest::get("/user"),
                        &RequestOptions::default()
                    ),
                    Err(GitHubError::Paused { .. })
                ));
            }
            "missing" => assert!(matches!(answer, Err(GitHubError::NotFound))),
            _ => unreachable!(),
        }
    }
    for (status, headers, body, limited) in [
        (429, "", "", true),
        (403, "x-ratelimit-remaining: 0\r\n", "", true),
        (403, "retry-after: 60\r\n", "", true),
        (403, "", "rate limit", true),
        (
            403,
            "",
            r#"{"message":"forbidden","errors":["detail",{"field":"ref","code":"invalid"}]}"#,
            false,
        ),
    ] {
        let fixture = Fixture::new();
        let store = Store::new();
        let api = GitHubApi::new(
            store.credentials(&[("GH_TOKEN", "fixture")]),
            fixture.builder(),
        );
        let (answer, _, _) = fixture.call(
            || {
                api.rest(
                    "github.com",
                    RestRequest::get("/user"),
                    &RequestOptions::default(),
                )
            },
            status,
            headers,
            body.as_bytes(),
        );
        if limited {
            assert!(matches!(answer, Err(GitHubError::RateLimited { .. })));
        } else {
            assert_eq!(
                answer.unwrap_err(),
                GitHubError::Response {
                    status: 403,
                    messages: vec!["forbidden".into(), "detail".into(), "ref invalid".into()]
                }
            );
        }
    }
}

#[test]
fn conditional_reads_reuse_the_validator_without_debiting_and_reserve_stays_interactive() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let window = reset();
    let headers = quota("core", 10, window) + "etag: \"same\"\r\n";
    let (first, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/checks"),
                &RequestOptions::default(),
            )
        },
        200,
        &headers,
        b"cached checks",
    );
    let first = first.unwrap();
    for _ in 0..12 {
        let mut request = RestRequest::get("/checks");
        request.if_none_match = first.headers.get("etag").map(String::as_str);
        let (answer, sent, _) = fixture.call(
            || api.rest("github.com", request, &RequestOptions::default()),
            304,
            "",
            b"",
        );
        assert_eq!(answer.unwrap().status, 304);
        assert!(
            sent.to_ascii_lowercase()
                .contains("if-none-match: \"same\"")
        );
    }
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/checks"),
                &RequestOptions::default(),
            )
        },
        304,
        "",
        b"",
    );
    assert!(matches!(
        answer,
        Err(GitHubError::Response { status: 304, .. })
    ));
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/checks"),
                &RequestOptions::default(),
            )
        },
        200,
        &quota("core", 9, window),
        b"{}",
    );
    answer.unwrap();
    assert!(matches!(
        api.rest(
            "github.com",
            RestRequest::get("/checks"),
            &RequestOptions::default()
        ),
        Err(GitHubError::Paused { .. })
    ));
    let interactive = RequestOptions {
        interactive: true,
        ..Default::default()
    };
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/checks"), &interactive),
        200,
        &quota("core", 1, window),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/checks"), &interactive),
        200,
        &quota("core", 0, window),
        b"{}",
    );
    answer.unwrap();
    assert!(matches!(
        api.rest("github.com", RestRequest::get("/checks"), &interactive),
        Err(GitHubError::Paused { .. })
    ));
    // A different resource and anonymous credential have independent quota snapshots.
    let (answer, _, _) = fixture.call(
        || api.graphql("github.com", &document(), &RequestOptions::default()),
        200,
        "",
        b"{\"data\":{}}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || tcode_services::version_check::ReleaseFeed::new(api.clone()).latest_json(),
        200,
        "",
        b"{}",
    );
    answer.unwrap();
}

#[test]
fn latest_arrival_can_restore_quota_but_an_older_success_cannot_clear_a_new_refusal() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let window = reset();
    thread::scope(|scope| {
        let old = scope.spawn(|| {
            api.rest(
                "github.com",
                RestRequest::get("/old"),
                &RequestOptions::default(),
            )
        });
        let old_exchange = fixture.next();
        let (answer, _, _) = fixture.call(
            || api.graphql("github.com", &document(), &RequestOptions::default()),
            200,
            "retry-after: 600\r\n",
            b"{\"errors\":[{\"type\":\"RATE_LIMITED\"}]}",
        );
        assert!(matches!(answer, Err(GitHubError::RateLimited { .. })));
        old_exchange.reply(200, &quota("core", 90, window), b"{}");
        old.join().unwrap().unwrap();
    });
    assert!(matches!(
        api.rest(
            "github.com",
            RestRequest::get("/next"),
            &RequestOptions::default()
        ),
        Err(GitHubError::Paused { .. })
    ));
    let interactive = RequestOptions {
        interactive: true,
        ..Default::default()
    };
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/next"), &interactive),
        200,
        &quota("core", 1, window),
        b"{}",
    );
    answer.unwrap();
    assert!(matches!(
        api.rest(
            "github.com",
            RestRequest::get("/next"),
            &RequestOptions::default()
        ),
        Err(GitHubError::Paused { .. })
    ));
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/next"), &interactive),
        200,
        &quota("core", 70, window),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/next"), &interactive),
        200,
        &quota("core", 0, window - 1),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/next"), &interactive),
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    store
        .store
        .set_token(
            tcode_core::pull_request::HostKind::Github,
            "github.com",
            Some("fresh-quota-scope"),
        )
        .unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/new"), &interactive),
        200,
        &quota("core", 1, window),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/new"), &interactive),
        200,
        &quota("core", 70, window),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/new"),
                &RequestOptions::default(),
            )
        },
        200,
        &quota("core", 0, window - 1),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/new"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
}

#[test]
fn pinned_verified_identity_is_cached_per_fingerprint_and_refuses_another_host() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let (verified, sent, _) = fixture.call(
        || api.verified_credential("github.com"),
        200,
        "",
        b"{\"id\":42,\"login\":\"fixture-user\"}",
    );
    assert!(sent.starts_with("GET /user "));
    let (credential, identity) = verified.unwrap();
    assert_eq!(identity.login, "fixture-user");
    assert_eq!(api.verified_credential("github.com").unwrap().1, identity);
    let options = RequestOptions {
        authentication: Authentication::Pinned(credential),
        ..Default::default()
    };
    assert_eq!(
        api.rest("other.example.com", RestRequest::get("/user"), &options)
            .unwrap_err(),
        GitHubError::Credential(CredentialError::HostMismatch)
    );
    store
        .store
        .set_token(
            tcode_core::pull_request::HostKind::Github,
            "github.com",
            Some("replacement"),
        )
        .unwrap();
    let (verified, _, _) = fixture.call(
        || api.verified_credential("github.com"),
        200,
        "",
        b"{\"id\":43,\"login\":\"other-user\"}",
    );
    assert_eq!(verified.unwrap().1.id, 43);
}

#[test]
fn body_cap_and_deadline_cover_streaming_after_headers() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/large"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        &vec![b'a'; 8 * 1024 * 1024 + 1],
    );
    let response = answer.unwrap();
    assert!(response.truncated);
    assert_eq!(response.body.len(), 8 * 1024 * 1024);
    assert_eq!(
        response.json::<serde_json::Value>().unwrap_err(),
        GitHubError::BodyTooLarge
    );
    let bounded = RequestOptions {
        body_limit: 16,
        ..Default::default()
    };
    let (answer, _, _) = fixture.call(
        || api.graphql("github.com", &document(), &bounded),
        200,
        "",
        b"{\"data\":{}}                 {\"errors\":[{}]}",
    );
    assert_eq!(answer.unwrap_err(), GitHubError::BodyTooLarge);
    let (answer, _, _) = fixture.call(
        || api.graphql("github.com", &document(), &bounded),
        429,
        "",
        b"a refusal whose body exceeds the cap",
    );
    assert!(matches!(
        answer,
        Err(GitHubError::RateLimited { status: 429, .. })
    ));
    let (answer, _, _) = fixture.call(
        || tcode_services::version_check::ReleaseFeed::new(api.clone()).latest_json(),
        200,
        "",
        &vec![b'a'; 1024 * 1024 + 1],
    );
    assert_eq!(
        answer.unwrap_err(),
        tcode_services::version_check::FetchError::ResponseTooLarge
    );
    let timeout = RequestOptions {
        timeout: Duration::from_millis(200),
        interactive: true,
        ..Default::default()
    };
    thread::scope(|scope| {
        let job = scope.spawn(|| api.rest("github.com", RestRequest::get("/slow"), &timeout));
        let mut exchange = fixture.next();
        exchange
            .stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
            .unwrap();
        // Keep the body open until the client reports its whole-response deadline.
        assert_eq!(job.join().unwrap().unwrap_err(), GitHubError::Deadline);
    });
}

#[test]
fn aliases_and_pager_send_values_as_variables_and_report_unread_tail() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let hostile = "owner\") { secret } #";
    let doc = graphql::aliases(
        "query",
        "Batch",
        "r",
        &[
            AliasItem {
                key: 0,
                variables: BTreeMap::from([(
                    "owner".into(),
                    ("String!".into(), serde_json::json!(hostile)),
                )]),
            },
            AliasItem {
                key: 1,
                variables: BTreeMap::from([(
                    "owner".into(),
                    ("String!".into(), serde_json::json!("other")),
                )]),
            },
        ],
        &BTreeMap::from([(
            "name".into(),
            ("String!".into(), serde_json::json!("shared-repo")),
        )]),
        |vars| format!("repository(owner: {}, name: $name) {{ id }}", vars["owner"]),
        |fields| fields,
    )
    .unwrap();
    let (answer, _, body) = fixture.call(
        || api.graphql("github.com", &doc, &RequestOptions::default()),
        200,
        "",
        b"{\"data\":{}}",
    );
    answer.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let query = sent["query"].as_str().unwrap();
    assert!(!query.contains(hostile));
    assert!(!query.contains("shared-repo"));
    assert_eq!(sent["variables"]["r0_owner"], hostile);
    assert_eq!(query.matches("$name:").count(), 1);
    for (cursors, max_pages, truncated) in [
        (vec![Some("a"), None], None, false),
        (vec![Some("a"), Some("a")], None, true),
        (vec![Some("a")], Some(1), true),
    ] {
        thread::scope(|scope| {
            let job = scope.spawn(|| graphql::pages(None, max_pages, |after, _| {
                let doc = Document { query: "query Page($after: String) { viewer { repositories(first: 1, after: $after) { pageInfo { endCursor } } } }".into(), variables: BTreeMap::from([("after".into(), serde_json::json!(after))]) };
                api.graphql("github.com", &doc, &RequestOptions::default())?.json::<serde_json::Value>()
            }, |page| page.get("next").and_then(|value| value.as_str()).map(str::to_owned), |_| false));
            let mut expected = None;
            for cursor in cursors {
                let exchange = fixture.next();
                let sent: serde_json::Value = serde_json::from_slice(&exchange.body).unwrap();
                assert_eq!(sent["variables"]["after"], serde_json::json!(expected));
                exchange.reply(
                    200,
                    "",
                    serde_json::json!({"data":{}, "next":cursor})
                        .to_string()
                        .as_bytes(),
                );
                expected = cursor;
            }
            assert_eq!(job.join().unwrap().unwrap().truncated, truncated);
        });
    }
}

#[cfg(unix)]
fn gh_fixture(store: &Store) -> Vec<(String, String)> {
    use std::os::unix::fs::PermissionsExt as _;
    let program = store.root.join("gh");
    std::fs::write(
        &program,
        r#"#!/bin/sh
[ "$GH_PROMPT_DISABLED" = 1 ] || exit 9
[ -z "$GH_DEBUG$GH_TOKEN$GITHUB_TOKEN$GH_ENTERPRISE_TOKEN$GITHUB_ENTERPRISE_TOKEN" ] || exit 9
printf '%s\n' "$*" >> "$FIXTURE_ROOT/calls"
if [ "$2" = status ]; then
  printf '%s' '{"hosts":{"github.com":[{"login":"first"},{"login":"second"}]}}'
  exit 0
fi
if [ "$6" = missing ]; then exit 1; fi
if [ -f "$FIXTURE_ROOT/signed-out" ]; then exit 1; fi
if [ -n "$6" ]; then printf '%s' "account-$6"; else /bin/cat "$FIXTURE_ROOT/token"; fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(store.root.join("token"), "cli-first").unwrap();
    vec![
        ("PATH".into(), store.root.to_string_lossy().into_owned()),
        (
            "FIXTURE_ROOT".into(),
            store.root.to_string_lossy().into_owned(),
        ),
    ]
}

#[cfg(unix)]
#[test]
fn gh_process_boundary_honors_binding_pins_cache_invalidation_and_failure_ttls() {
    use std::os::unix::fs::PermissionsExt as _;
    use tcode_core::settings::CredentialSource;
    use tcode_services::github::Credentials;
    let fixture = Fixture::new();
    let store = Store::new();
    let mut env = gh_fixture(&store);
    env.extend([
        ("GH_ENTERPRISE_TOKEN".into(), "enterprise-secret".into()),
        ("GH_HOST".into(), "allowed.example.com".into()),
    ]);
    let credentials = Credentials::new(store.store.clone(), env);
    let api = GitHubApi::new(credentials.clone(), fixture.builder());
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "untrusted.example.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer cli-first"));
    assert!(!request.contains("enterprise-secret"));
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "allowed.example.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer enterprise-secret"));
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    std::fs::write(store.root.join("token"), "cli-next").unwrap();
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        401,
        "",
        b"{}",
    );
    assert_eq!(answer.unwrap_err(), GitHubError::Unauthorized);
    assert!(request.contains("Bearer cli-first"));
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer cli-next"));
    for (account, token) in [
        ("first", "account-first"),
        ("second", "account-second"),
        ("missing", "cli-next"),
    ] {
        credentials.configure(BTreeMap::from([(
            "github.com".into(),
            HostSettings {
                kind: HostKind::Github,
                enabled: true,
                account: Some(account.into()),
            },
        )]));
        let (answer, request, _) = fixture.call(
            || {
                api.rest(
                    "github.com",
                    RestRequest::get("/user"),
                    &RequestOptions::default(),
                )
            },
            200,
            "",
            b"{}",
        );
        answer.unwrap();
        assert!(request.contains(&format!("Bearer {token}")));
    }
    let mut env = gh_fixture(&store);
    env.extend([
        ("GH_TOKEN".into(), " ".into()),
        ("GITHUB_TOKEN".into(), "environment-fallback".into()),
    ]);
    let env_credentials = Credentials::new(store.store.clone(), env);
    env_credentials.configure(BTreeMap::from([(
        "github.com".into(),
        HostSettings {
            kind: HostKind::Github,
            enabled: true,
            account: Some("second".into()),
        },
    )]));
    let env_api = GitHubApi::new(env_credentials.clone(), fixture.builder());
    let (answer, request, _) = fixture.call(
        || {
            env_api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer environment-fallback"));
    let status = env_credentials.discover();
    assert!(status["github.com"].env_overrides_account);
    assert_eq!(status["github.com"].accounts, ["first", "second"]);
    assert_eq!(
        status["github.com"].source,
        Some(CredentialSource::Env {
            name: "GITHUB_TOKEN".into()
        })
    );
    // A negative CLI lookup is cached; a repaired executable failure is retried immediately.
    let retry_credentials = Credentials::new(store.store.clone(), gh_fixture(&store));
    let retry_api = GitHubApi::new(retry_credentials.clone(), fixture.builder());
    std::fs::set_permissions(
        store.root.join("gh"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        retry_api
            .rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default()
            )
            .unwrap_err(),
        GitHubError::Credential(CredentialError::CliFailed)
    );
    std::fs::set_permissions(
        store.root.join("gh"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let (answer, _, _) = fixture.call(
        || {
            retry_api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    retry_credentials.invalidate("github.com");
    std::fs::write(store.root.join("signed-out"), "").unwrap();
    assert_eq!(
        retry_api
            .rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default()
            )
            .unwrap_err(),
        GitHubError::Credential(CredentialError::NotSignedIn)
    );
    std::fs::remove_file(store.root.join("signed-out")).unwrap();
    assert_eq!(
        retry_api
            .rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default()
            )
            .unwrap_err(),
        GitHubError::Credential(CredentialError::NotSignedIn)
    );
    assert!(
        std::fs::read_to_string(store.root.join("calls"))
            .unwrap()
            .contains("auth token --hostname github.com --user second")
    );
}

#[test]
#[allow(clippy::disallowed_methods)] // Drive the same blocking-job cancellation used by HostCx::unblock.
fn dropped_waiters_keep_all_eight_socket_permits_until_their_jobs_finish() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let mut tasks = Vec::new();
    let mut exchanges = Vec::new();
    for _ in 0..8 {
        let api = api.clone();
        tasks.push(smol::spawn(async move {
            smol::unblock(move || {
                api.rest(
                    "github.com",
                    RestRequest::get("/held"),
                    &RequestOptions::default(),
                )
            })
            .await
        }));
        exchanges.push(fixture.next());
    }
    // Dropping every async owner leaves eight real sockets held by blocking jobs.
    drop(tasks);
    let (started_tx, started_rx) = mpsc::channel();
    let next_api = api.clone();
    let next = thread::spawn(move || {
        started_tx.send(()).unwrap();
        next_api.rest(
            "github.com",
            RestRequest::get("/ninth"),
            &RequestOptions::default(),
        )
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        fixture.incoming.recv_timeout(Duration::from_millis(200)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    exchanges.pop().unwrap().reply(200, "", b"{}");
    let ninth = fixture.next();
    assert!(ninth.request.starts_with("GET /ninth "));
    ninth.reply(200, "", b"{}");
    next.join().unwrap().unwrap();
    for exchange in exchanges {
        exchange.reply(200, "", b"{}");
    }
    // A round-trip after releasing all sockets ensures no abandoned I/O or fixture threads.
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/finished"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
}
#[test]
fn linked_pr_summaries_coalesce_and_retry_only_missing_graphql_aliases() {
    use tcode_core::pull_request::PullRequestKey;
    use tcode_services::github::pull_requests::PullRequests;
    let store = Store::new();
    let fixture = Fixture::new();
    let service = PullRequests::new(GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    ));
    let sent = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let responding = sent.clone();
    let _server=fixture.serve(move |exchange| {
        let body:serde_json::Value=serde_json::from_slice(&exchange.body).unwrap();
        let mut requests=responding.lock().unwrap();
        let first=requests.is_empty();
        requests.push(body.clone());
        let mut data=serde_json::Map::new();
        for (variable,number) in body["variables"].as_object().unwrap() {
            let Some(alias)=variable.strip_suffix("_number") else {continue};
            let owner=body["variables"][format!("{alias}_owner")].as_str().unwrap();
            let name=body["variables"][format!("{alias}_name")].as_str().unwrap();
            data.insert(alias.into(),if first && number==2 {serde_json::Value::Null}else{serde_json::json!({"pullRequest":{"number":number,"url":format!("https://github.com/{owner}/{name}/pull/{number}"),"title":"A real summary","state":"OPEN","headRefName":"feature","baseRefName":"main","updatedAt":"2026-10-08T00:00:00Z","reviewDecision":"REVIEW_REQUIRED","latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"reviewer"}}]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]},"stack":null}})});
        }
        exchange.reply(200,"",&serde_json::to_vec(&serde_json::json!({"data":data})).unwrap());
    });
    let barrier = std::sync::Barrier::new(2);
    let rows = thread::scope(|scope| {
        let jobs: Vec<_> = (1..=2)
            .map(|number| {
                let barrier = &barrier;
                let service = &service;
                scope.spawn(move || {
                    barrier.wait();
                    service
                        .summary(&PullRequestKey::new(
                            "github.com",
                            if number == 1 {
                                "sample/one"
                            } else {
                                "sample/two"
                            },
                            number,
                        ))
                        .unwrap()
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| job.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(rows.iter().all(|row| row.snapshot.checks_state
        == Some(tcode_core::pull_request::ChecksState::Passing)
        && row.snapshot.review_decision
            == Some(tcode_core::pull_request::ReviewDecision::Approved)
        && row.stack_number == Some(None)));
    let requests = sent.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "only the missing summary needs a single GraphQL fallback"
    );
}

#[test]
fn stacks_are_unknown_off_github_com() {
    use tcode_services::github::pull_requests::PullRequests;
    let store = Store::new();
    let fixture = Fixture::new();
    let service = PullRequests::new(GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    ));
    assert!(matches!(
        service
            .stack(&tcode_core::pull_request::PullRequestKey::new(
                "github.example.com",
                "sample/project",
                5
            ))
            .unwrap(),
        tcode_core::pull_request::PullRequestStackState::Unknown
    ));
}

#[test]
fn real_git_checkouts_resolve_the_gh_default_remote() {
    use tcode_services::github::repository;
    let store = Store::new();
    let cwd = store.root.join("checkout");
    std::fs::create_dir(&cwd).unwrap();
    let git = |args: &[&str]| {
        let output = tcode_services::process::command("git")
            .current_dir(&cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&[
        "remote",
        "add",
        "upstream",
        "https://github.com/sample/project.git",
    ]);
    git(&[
        "remote",
        "add",
        "origin",
        "git@github-work:ForkOwner/project.git",
    ]);
    git(&[
        "remote",
        "add",
        "gitlab",
        "https://gitlab.example.com/sample/project.git",
    ]);
    git(&["config", "remote.upstream.gh-resolved", "base"]);
    let subdirectory = cwd.join("nested");
    std::fs::create_dir(&subdirectory).unwrap();
    assert_eq!(
        repository::resolve(&tcode_services::forge::Checkout::new(&subdirectory))
            .unwrap()
            .owner,
        "sample"
    );
    git(&["config", "--unset", "remote.upstream.gh-resolved"]);
    git(&["config", "remote.origin.gh-resolved", "base"]);
    assert_eq!(
        repository::resolve(&tcode_services::forge::Checkout::new(&subdirectory))
            .unwrap()
            .owner,
        "sample",
        "mixed hosts and SSH aliases reject the strict gh mark and use the host-ranked remote"
    );
}
