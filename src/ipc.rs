//! IPC protocol between `snypr` clients and a running `snypr daemon`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cli::ClipboardKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Screenshot(ScreenshotRequest),
    DrawToggle,
    /// Flip pointer passthrough on the currently running daemon-managed draw overlay.
    /// Errors when no overlay is alive. Bind to a Hyprland global keybind so users can
    /// recover from passthrough mode (which detaches the surface from the keyboard).
    PassthroughToggle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotRequest {
    pub selection: SelectionSpec,
    pub cursor: bool,
    /// Open the annotation editor between capture and sinks. Defaults to `false` so legacy
    /// clients (and tests) keep parsing.
    #[serde(default)]
    pub edit: bool,
    /// Delay before capture, in whole seconds. `None` (or omitted) skips the sleep
    /// entirely. The UI countdown only operates on integer seconds, so the wire format
    /// matches the CLI / config representation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_secs: Option<u32>,
    pub sinks: Vec<SinkSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionSpec {
    Full,
    PerOutput,
    Focused,
    Output { name: String },
    Window,
    Region { x: i32, y: i32, w: u32, h: u32 },
    Interactive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SinkSpec {
    File {
        path: Option<PathBuf>,
    },
    /// Wayland clipboard sink. `clipboard_kind = None` means the daemon
    /// should fall back to its own configured default; `Some(kind)` pins
    /// the kind for this entry.
    Clipboard {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        clipboard_kind: Option<ClipboardKind>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Paths { paths: Vec<PathBuf> },
    Error { message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("malformed IPC frame: {0}")]
    Malformed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[rstest]
    #[case(Request::Ping)]
    #[case(Request::DrawToggle)]
    #[case(Request::PassthroughToggle)]
    #[case(Request::Screenshot(ScreenshotRequest {
        selection: SelectionSpec::Full,
        cursor: false,
        edit: false,
        delay_secs: None,
        sinks: vec![SinkSpec::Clipboard { clipboard_kind: None }],
    }))]
    #[case(Request::Screenshot(ScreenshotRequest {
        selection: SelectionSpec::Full,
        cursor: false,
        edit: false,
        delay_secs: None,
        sinks: vec![SinkSpec::Clipboard { clipboard_kind: Some(ClipboardKind::Primary) }],
    }))]
    #[case(Request::Screenshot(ScreenshotRequest {
        selection: SelectionSpec::Interactive,
        cursor: true,
        edit: true,
        delay_secs: Some(3),
        sinks: vec![SinkSpec::File { path: None }],
    }))]
    fn round_trips_requests(#[case] req: Request) {
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        // Round-trip equality via JSON since `Request` doesn't derive PartialEq.
        let again = serde_json::to_string(&back).unwrap();
        assert_eq!(json, again);
    }

    #[rstest]
    #[case(Response::Ok)]
    #[case(Response::Paths { paths: vec!["/tmp/a.png".into()] })]
    #[case(Response::Error { message: "boom".into() })]
    fn round_trips_responses(#[case] resp: Response) {
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        let again = serde_json::to_string(&back).unwrap();
        assert_eq!(json, again);
    }

    /// The round-trip tests above compare serialisation to serialisation, so they stay green
    /// even if a field is dropped in *both* directions. Pin the exact wire bytes instead.
    #[rstest]
    #[case(Request::Ping, r#"{"kind":"ping"}"#)]
    #[case(Request::DrawToggle, r#"{"kind":"draw_toggle"}"#)]
    #[case(Request::PassthroughToggle, r#"{"kind":"passthrough_toggle"}"#)]
    fn unit_requests_serialise_to_snake_case_tags(#[case] req: Request, #[case] expected: &str) {
        assert_eq!(serde_json::to_string(&req).unwrap(), expected);
    }

    #[test]
    fn screenshot_request_omits_optional_fields_when_unset() {
        let json = serde_json::to_string(&Request::Screenshot(ScreenshotRequest {
            selection: SelectionSpec::Full,
            cursor: false,
            edit: false,
            delay_secs: None,
            sinks: vec![SinkSpec::Clipboard {
                clipboard_kind: None,
            }],
        }))
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"screenshot","selection":{"kind":"full"},"cursor":false,"edit":false,"sinks":[{"kind":"clipboard"}]}"#
        );
    }

    #[test]
    fn screenshot_request_emits_optional_fields_when_set() {
        let json = serde_json::to_string(&Request::Screenshot(ScreenshotRequest {
            selection: SelectionSpec::Region {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
            },
            cursor: true,
            edit: true,
            delay_secs: Some(3),
            sinks: vec![
                SinkSpec::File {
                    path: Some("/tmp/a.png".into()),
                },
                SinkSpec::Clipboard {
                    clipboard_kind: Some(ClipboardKind::Primary),
                },
            ],
        }))
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"screenshot","selection":{"kind":"region","x":1,"y":2,"w":3,"h":4},"cursor":true,"edit":true,"delay_secs":3,"sinks":[{"kind":"file","path":"/tmp/a.png"},{"kind":"clipboard","clipboard_kind":"primary"}]}"#
        );
    }

    #[rstest]
    #[case(SelectionSpec::Full, r#"{"kind":"full"}"#)]
    #[case(SelectionSpec::PerOutput, r#"{"kind":"per_output"}"#)]
    #[case(SelectionSpec::Focused, r#"{"kind":"focused"}"#)]
    #[case(SelectionSpec::Output { name: "DP-1".into() }, r#"{"kind":"output","name":"DP-1"}"#)]
    #[case(SelectionSpec::Window, r#"{"kind":"window"}"#)]
    #[case(SelectionSpec::Interactive, r#"{"kind":"interactive"}"#)]
    fn selection_specs_use_snake_case_tags(#[case] spec: SelectionSpec, #[case] expected: &str) {
        assert_eq!(serde_json::to_string(&spec).unwrap(), expected);
    }

    #[rstest]
    #[case(Response::Ok, r#"{"kind":"ok"}"#)]
    #[case(Response::Paths { paths: vec!["/tmp/a.png".into()] }, r#"{"kind":"paths","paths":["/tmp/a.png"]}"#)]
    #[case(Response::Error { message: "boom".into() }, r#"{"kind":"error","message":"boom"}"#)]
    fn responses_use_snake_case_tags(#[case] resp: Response, #[case] expected: &str) {
        assert_eq!(serde_json::to_string(&resp).unwrap(), expected);
    }

    /// `edit`, `delay_secs` and `clipboard_kind` all carry `#[serde(default)]` specifically so
    /// frames written by older clients keep deserialising. Pin that contract.
    #[test]
    fn legacy_frames_without_the_optional_fields_still_parse() {
        let legacy = r#"{"kind":"screenshot","selection":{"kind":"full"},"cursor":false,"sinks":[{"kind":"clipboard"},{"kind":"file","path":null}]}"#;
        let Request::Screenshot(req) = serde_json::from_str::<Request>(legacy).unwrap() else {
            panic!("expected a screenshot request");
        };
        assert!(!req.edit);
        assert_eq!(req.delay_secs, None);
        assert!(matches!(
            req.sinks.as_slice(),
            [
                SinkSpec::Clipboard {
                    clipboard_kind: None
                },
                SinkSpec::File { path: None }
            ]
        ));
    }

    #[test]
    fn an_unknown_request_kind_is_rejected() {
        let err = serde_json::from_str::<Request>(r#"{"kind":"launch_missiles"}"#).unwrap_err();
        assert!(
            err.to_string().contains("launch_missiles"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn protocol_error_wraps_json_failures() {
        let err = serde_json::from_str::<Request>("{ not json").unwrap_err();
        let wrapped = ProtocolError::from(err);
        assert!(matches!(wrapped, ProtocolError::Json(_)));
        assert_eq!(
            ProtocolError::Malformed("no trailing newline".into()).to_string(),
            "malformed IPC frame: no trailing newline"
        );
    }
}
