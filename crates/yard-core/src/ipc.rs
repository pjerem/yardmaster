//! IPC wire protocol shared by the daemon and its clients (CLI, TUI).
//!
//! Transport: JSON-lines over a unix socket at `{state_dir}/daemon.sock`
//! (one JSON document per `\n`-terminated line). Serde types only — no I/O
//! here; the server lives in `yard-daemon::server`.
//!
//! Wire format (exact shapes, asserted by the tests below):
//! - server greeting on connect: `{"hello":{"version":"0.0.1","proto":1,"pid":123}}`
//! - request:  `{"id":1,"method":"ping"}` (optional reserved `"params":{…}`)
//! - response: `{"id":1,"ok":true,"data":<value>}` or `{"id":1,"ok":false,"error":"<msg>"}`
//! - event (after a successful `subscribe`): `{"event":"<kind>","payload":<value>}`

use serde::de::Error as _;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Version of the JSON-lines protocol itself, sent in the hello greeting.
pub const PROTO_VERSION: u32 = 1;

/// Payload of the greeting the server sends on every new connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: String,
    pub proto: u32,
    pub pid: u32,
}

/// The greeting line as it appears on the wire: `{"hello":{…}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloFrame {
    pub hello: Hello,
}

/// Client request. `params` is reserved for future methods; today's methods
/// take none and clients may omit the field entirely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: Method,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Ping,
    Status,
    Subscribe,
    Shutdown,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Ping => "ping",
            Method::Status => "status",
            Method::Subscribe => "subscribe",
            Method::Shutdown => "shutdown",
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Server response to a [`Request`], keyed by the request `id`.
///
/// Wire shape is `ok`-discriminated (`data` xor `error`), which doesn't map
/// onto derived serde, hence the manual impls below.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: u64,
    pub result: Result<Value, String>,
}

impl Response {
    pub fn ok(id: u64, data: Value) -> Self {
        Self {
            id,
            result: Ok(data),
        }
    }

    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            result: Err(error.into()),
        }
    }
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("Response", 3)?;
        st.serialize_field("id", &self.id)?;
        match &self.result {
            Ok(data) => {
                st.serialize_field("ok", &true)?;
                st.serialize_field("data", data)?;
            }
            Err(error) => {
                st.serialize_field("ok", &false)?;
                st.serialize_field("error", error)?;
            }
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            id: u64,
            ok: bool,
            #[serde(default)]
            data: Option<Value>,
            #[serde(default)]
            error: Option<String>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let result = if raw.ok {
            if raw.error.is_some() {
                return Err(D::Error::custom("ok response must not carry `error`"));
            }
            Ok(raw.data.unwrap_or(Value::Null))
        } else {
            if raw.data.is_some() {
                return Err(D::Error::custom("error response must not carry `data`"));
            }
            Err(raw.error.ok_or_else(|| D::Error::missing_field("error"))?)
        };
        Ok(Response { id: raw.id, result })
    }
}

/// Pushed by the server on subscribed connections (never in reply to an id).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventMsg {
    pub event: String,
    pub payload: Value,
}

/// `data` payload of a successful `status` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    pub daemon: DaemonInfo,
    pub items: Vec<WorkItemStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub pid: u32,
    /// RFC3339 UTC.
    pub started_at: String,
    pub state_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemStatus {
    pub id: i64,
    pub ticket: String,
    pub repo: String,
    pub state: String,
    pub agent: Option<String>,
    pub pending_gates: u32,
    pub mergeable: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hello_frame_wire_format() {
        let frame = HelloFrame {
            hello: Hello {
                version: "0.0.1".into(),
                proto: PROTO_VERSION,
                pid: 1234,
            },
        };
        let wire = serde_json::to_string(&frame).unwrap();
        assert_eq!(
            wire,
            r#"{"hello":{"version":"0.0.1","proto":1,"pid":1234}}"#
        );
        assert_eq!(serde_json::from_str::<HelloFrame>(&wire).unwrap(), frame);
    }

    #[test]
    fn request_without_params_omits_field() {
        let req = Request {
            id: 1,
            method: Method::Ping,
            params: None,
        };
        let wire = serde_json::to_string(&req).unwrap();
        assert_eq!(wire, r#"{"id":1,"method":"ping"}"#);
        assert_eq!(serde_json::from_str::<Request>(&wire).unwrap(), req);
    }

    #[test]
    fn request_with_params_round_trips() {
        let req = Request {
            id: 42,
            method: Method::Status,
            params: Some(json!({"verbose": true})),
        };
        let wire = serde_json::to_string(&req).unwrap();
        assert_eq!(
            wire,
            r#"{"id":42,"method":"status","params":{"verbose":true}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&wire).unwrap(), req);
    }

    #[test]
    fn methods_serialize_lowercase() {
        for (method, name) in [
            (Method::Ping, "ping"),
            (Method::Status, "status"),
            (Method::Subscribe, "subscribe"),
            (Method::Shutdown, "shutdown"),
        ] {
            assert_eq!(
                serde_json::to_string(&method).unwrap(),
                format!("\"{name}\"")
            );
            assert_eq!(
                serde_json::from_str::<Method>(&format!("\"{name}\"")).unwrap(),
                method
            );
        }
        assert!(serde_json::from_str::<Method>("\"reboot\"").is_err());
    }

    #[test]
    fn ok_response_wire_format() {
        let resp = Response::ok(7, json!({"pong": true}));
        let wire = serde_json::to_string(&resp).unwrap();
        assert_eq!(wire, r#"{"id":7,"ok":true,"data":{"pong":true}}"#);
        assert_eq!(serde_json::from_str::<Response>(&wire).unwrap(), resp);
    }

    #[test]
    fn error_response_wire_format() {
        let resp = Response::err(8, "boom");
        let wire = serde_json::to_string(&resp).unwrap();
        assert_eq!(wire, r#"{"id":8,"ok":false,"error":"boom"}"#);
        assert_eq!(serde_json::from_str::<Response>(&wire).unwrap(), resp);
    }

    #[test]
    fn error_response_without_message_is_rejected() {
        assert!(serde_json::from_str::<Response>(r#"{"id":1,"ok":false}"#).is_err());
    }

    #[test]
    fn event_msg_wire_format() {
        let ev = EventMsg {
            event: "item_updated".into(),
            payload: json!({"id": 3}),
        };
        let wire = serde_json::to_string(&ev).unwrap();
        assert_eq!(wire, r#"{"event":"item_updated","payload":{"id":3}}"#);
        assert_eq!(serde_json::from_str::<EventMsg>(&wire).unwrap(), ev);
    }

    #[test]
    fn status_report_round_trips() {
        let report = StatusReport {
            daemon: DaemonInfo {
                version: "0.0.1".into(),
                pid: 99,
                started_at: "2026-09-22T10:00:00Z".into(),
                state_dir: "/tmp/state".into(),
            },
            items: vec![WorkItemStatus {
                id: 1,
                ticket: "ZEE-12".into(),
                repo: "backend".into(),
                state: "Developing".into(),
                agent: None,
                pending_gates: 2,
                mergeable: Some(false),
            }],
        };
        let wire = serde_json::to_string(&report).unwrap();
        assert_eq!(serde_json::from_str::<StatusReport>(&wire).unwrap(), report);
    }
}
