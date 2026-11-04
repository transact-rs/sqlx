// This is imported by different tests that use different functions.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{request, Request};
use axum::response::Response;
use http_body_util::BodyExt;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

pub trait RequestBuilderExt {
    fn json(self, json: serde_json::Value) -> Request<Body>;

    fn empty_body(self) -> Request<Body>;
}

impl RequestBuilderExt for request::Builder {
    fn json(self, json: serde_json::Value) -> Request<Body> {
        self.header("Content-Type", "application/json")
            .body(Body::from(json.to_string()))
            .expect("failed to build request")
    }

    fn empty_body(self) -> Request<Body> {
        self.body(Body::empty()).expect("failed to build request")
    }
}

pub async fn response_json(resp: &mut Response) -> serde_json::Value {
    assert_eq!(
        resp.headers()
            .get(CONTENT_TYPE)
            .expect("expected Content-Type"),
        "application/json"
    );

    let bytes = resp
        .collect()
        .await
        .expect("error reading response body")
        .to_bytes();

    serde_json::from_slice(&bytes).expect("failed to read response body as json")
}

#[track_caller]
pub fn expect_string(value: &serde_json::Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("expected string, got {value:?}"))
}

#[track_caller]
pub fn expect_uuid(value: &serde_json::Value) -> Uuid {
    expect_string(value)
        .parse::<Uuid>()
        .unwrap_or_else(|e| panic!("failed to parse UUID from {value:?}: {e}"))
}

#[track_caller]
pub fn expect_rfc3339_timestamp(value: &serde_json::Value) -> OffsetDateTime {
    let s = expect_string(value);

    OffsetDateTime::parse(s, &Rfc3339)
        .unwrap_or_else(|e| panic!("failed to parse RFC-3339 timestamp from {value:?}: {e}"))
}
