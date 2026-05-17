//! IPC protocol between `hyprsnap` clients and a running `hyprsnap daemon`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    File { path: Option<PathBuf> },
    Clipboard,
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
        sinks: vec![SinkSpec::Clipboard],
    }))]
    #[case(Request::Screenshot(ScreenshotRequest {
        selection: SelectionSpec::Interactive,
        cursor: true,
        edit: true,
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
}
