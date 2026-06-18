// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wire protocol for the privileged helper.
//!
//! The unprivileged main MIRA and the root `mira-helper` daemon speak
//! newline-delimited JSON over a unix socket: one [`Request`] per line, one
//! [`Response`] per line. The op set is a **fixed enum** — there is deliberately
//! no "run this command" — so the privileged surface is exactly the operations
//! enumerated here, each validated by the daemon before it acts.

use serde::{Deserialize, Serialize};

/// Default socket path (under systemd's `RuntimeDirectory=mira-helper`).
pub const DEFAULT_SOCKET: &str = "/run/mira-helper/sock";

/// A request from the unprivileged client to the privileged daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Health check — proves the channel works and reports whether the daemon
    /// actually holds `CAP_NET_ADMIN`. Side-effect free.
    Ping,
    /// Provision native-tier egress filtering for a confined subprocess: wire a
    /// veth into `pid`'s netns and install host NAT + an nftables drop-default
    /// filter whose allowed set is populated by a dnsmasq restricted to `allow`.
    /// `upstream` is the resolver IP dnsmasq forwards to (defaults to 1.1.1.1).
    NetAllow {
        pid: u32,
        allow: Vec<String>,
        #[serde(default)]
        upstream: Option<String>,
    },
    /// Tear down the egress slot previously set up for `pid`. Idempotent.
    NetTeardown { pid: u32 },
}

/// The daemon's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self { ok: true, error: None, data: Some(data) }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), data: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_round_trips_as_tagged_json() {
        let line = serde_json::to_string(&Request::Ping).unwrap();
        assert_eq!(line, r#"{"op":"ping"}"#);
        let back: Request = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, Request::Ping));
    }

    #[test]
    fn net_ops_round_trip_as_tagged_json() {
        let allow = Request::NetAllow {
            pid: 4242,
            allow: vec!["api.example.com".into()],
            upstream: Some("1.1.1.1".into()),
        };
        let line = serde_json::to_string(&allow).unwrap();
        assert!(line.contains(r#""op":"net_allow""#) && line.contains("4242"));
        match serde_json::from_str::<Request>(&line).unwrap() {
            Request::NetAllow { pid, allow, upstream } => {
                assert_eq!(pid, 4242);
                assert_eq!(allow, vec!["api.example.com".to_string()]);
                assert_eq!(upstream.as_deref(), Some("1.1.1.1"));
            }
            _ => panic!("wrong variant"),
        }
        // upstream is optional on the wire.
        let r: Request =
            serde_json::from_str(r#"{"op":"net_allow","pid":7,"allow":["a.b"]}"#).unwrap();
        assert!(matches!(r, Request::NetAllow { upstream: None, .. }));
        let t: Request = serde_json::from_str(r#"{"op":"net_teardown","pid":7}"#).unwrap();
        assert!(matches!(t, Request::NetTeardown { pid: 7 }));
    }

    #[test]
    fn unknown_op_is_rejected_at_deserialize() {
        let r: Result<Request, _> = serde_json::from_str(r#"{"op":"rm_rf_slash"}"#);
        assert!(r.is_err(), "unknown ops must not deserialize into a variant");
    }

    #[test]
    fn response_helpers_shape_json() {
        let ok = serde_json::to_string(&Response::ok(serde_json::json!({"pong": true}))).unwrap();
        assert!(ok.contains("\"ok\":true") && ok.contains("pong"));
        let err = serde_json::to_string(&Response::err("nope")).unwrap();
        assert!(err.contains("\"ok\":false") && err.contains("nope"));
    }
}
