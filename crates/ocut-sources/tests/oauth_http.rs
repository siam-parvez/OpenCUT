//! OAuth source against a local wiremock server. Never touches the real
//! endpoint or `~/.claude`.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use ocut_sources::LimitReadings;
use ocut_sources::Source;
use ocut_sources::credentials::{AccessToken, parse_credentials_json};
use ocut_sources::oauth::{BETA_HEADER, OAuthClient, OAuthError, OAuthSource};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CANARY: &str = "sk-ant-oat01-CANARY-c4n4ry-never-print-me";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
}

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/oauth")
        .join(name);
    std::fs::read_to_string(p).unwrap()
}

/// Mounts one response for a correctly authenticated request.
async fn server(resp: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .and(header("authorization", format!("Bearer {CANARY}").as_str()))
        .and(header(BETA_HEADER.0, BETA_HEADER.1))
        .respond_with(resp)
        .expect(1)
        .mount(&server)
        .await;
    server
}

/// `fetch_usage` is blocking, so run it off the async runtime.
async fn fetch(server: &MockServer) -> Result<LimitReadings, OAuthError> {
    let client = OAuthClient::with_url_for_tests(format!("{}/api/oauth/usage", server.uri()));
    let token = AccessToken::new(CANARY.into());
    let result = tokio::task::spawn_blocking(move || client.fetch_usage(&token, now()))
        .await
        .unwrap();
    assert_no_canary(&result);
    result
}

fn assert_no_canary<T: std::fmt::Debug>(r: &Result<T, OAuthError>) {
    let mut s = format!("{r:?}");
    if let Err(e) = r {
        s.push_str(&e.to_string());
    }
    assert!(!s.contains("CANARY"), "token leaked: {s}");
}

fn ts(s: &str) -> Option<DateTime<Utc>> {
    Some(DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc))
}

#[tokio::test]
async fn ok_flat_shape() {
    let s = server(ResponseTemplate::new(200).set_body_string(fixture("usage_flat.json"))).await;
    let r = fetch(&s).await.unwrap();
    let five = r.session_5h.unwrap();
    assert_eq!(five.pct, 62.0);
    assert_eq!(five.resets_at, ts("2026-10-11T14:00:00.123456Z"));
    assert_eq!(five.fetched_at, now());
    let week = r.weekly_all.unwrap();
    assert_eq!(week.pct, 34.0);
    assert_eq!(week.resets_at, Utc.timestamp_opt(1_792_072_800, 0).single());
    assert_eq!(r.weekly_opus.unwrap().pct, 12.5);
    assert!(r.weekly_sonnet.is_none());
    let extra = r.extra_usage.unwrap();
    assert!(extra.enabled);
    assert_eq!(extra.used_usd, 12.34);
    assert_eq!(extra.limit_usd, Some(50.0));
}

#[tokio::test]
async fn ok_limits_shape() {
    let s = server(ResponseTemplate::new(200).set_body_string(fixture("usage_limits.json"))).await;
    let r = fetch(&s).await.unwrap();
    let five = r.session_5h.unwrap();
    assert_eq!(five.pct, 81.0);
    assert_eq!(five.resets_at, Utc.timestamp_opt(1_792_076_400, 0).single());
    assert_eq!(r.weekly_all.unwrap().pct, 40.5);
    let opus = r.weekly_opus.unwrap();
    assert_eq!(
        (opus.pct, opus.resets_at),
        (22.0, ts("2026-10-15T08:00:00Z"))
    );
    let sonnet = r.weekly_sonnet.unwrap();
    assert_eq!((sonnet.pct, sonnet.resets_at), (7.0, None));
    assert!(r.extra_usage.is_none());
}

#[tokio::test]
async fn unauthorized_is_token_expired() {
    let s = server(ResponseTemplate::new(401).set_body_string(r#"{"error":"bad"}"#)).await;
    let e = fetch(&s).await.unwrap_err();
    assert!(matches!(e, OAuthError::TokenExpired));
    assert_eq!(e.code(), "token_expired");
}

#[tokio::test]
async fn rate_limited_with_retry_after() {
    let s = server(ResponseTemplate::new(429).insert_header("retry-after", "300")).await;
    match fetch(&s).await.unwrap_err() {
        OAuthError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(Duration::from_secs(300)))
        }
        e => panic!("unexpected {e:?}"),
    }
}

#[tokio::test]
async fn server_error_with_http_date_retry_after() {
    let s = server(
        ResponseTemplate::new(503).insert_header("retry-after", "Sun, 11 Oct 2026 12:10:00 GMT"),
    )
    .await;
    match fetch(&s).await.unwrap_err() {
        OAuthError::Server {
            status,
            retry_after,
        } => {
            assert_eq!(status, 503);
            assert_eq!(retry_after, Some(Duration::from_secs(600)));
        }
        e => panic!("unexpected {e:?}"),
    }
}

#[tokio::test]
async fn unknown_fields_only_is_schema_drift() {
    let body = format!(r#"{{"totally_new_shape":{{"pct":1}},"echo":"{CANARY}"}}"#);
    let s = server(ResponseTemplate::new(200).set_body_string(body)).await;
    let e = fetch(&s).await.unwrap_err();
    assert_eq!(e.code(), "schema_drift");
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let s =
        server(ResponseTemplate::new(302).insert_header("location", "http://127.0.0.1:9/steal"))
            .await;
    assert!(matches!(
        fetch(&s).await.unwrap_err(),
        OAuthError::Status { status: 302 }
    ));
}

#[tokio::test]
async fn canary_never_appears_in_debug_or_display() {
    let json = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{CANARY}","expiresAt":1792072800000,"subscriptionType":"pro"}}}}"#
    );
    let creds = parse_credentials_json(&json).unwrap();
    let src = OAuthSource::new(OAuthClient::new(), &creds);
    let rendered = format!(
        "{creds:?} {src:?} {} {:?} {}",
        creds.access_token, creds.access_token, src.token
    );
    assert!(!rendered.contains("CANARY"), "token leaked: {rendered}");
    assert_eq!(src.name(), "oauth_api");

    // Network failure path (nothing listening on the discard port).
    let client = OAuthClient::with_url_for_tests("http://127.0.0.1:9/api/oauth/usage");
    let token = creds.access_token.clone();
    let r = tokio::task::spawn_blocking(move || client.fetch_usage(&token, now()))
        .await
        .unwrap();
    assert!(matches!(r, Err(OAuthError::Network(_))));
    assert_no_canary(&r);
}
