//! Long-lived IPC daemon listening on a Unix socket.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::context::Ctx;
use crate::ipc::{Request, Response};

/// Default IPC socket path: `$XDG_RUNTIME_DIR/hyprsnap.sock`.
pub fn default_socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("hyprsnap.sock")
}

pub async fn serve(ctx: Ctx, socket: PathBuf) -> Result<()> {
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("binding socket {}", socket.display()))?;
    tracing::info!(path = %socket.display(), "hyprsnap daemon listening");

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(ctx, stream).await {
                                tracing::warn!(error = ?err, "daemon client error");
                            }
                        });
                    }
                    Err(err) => tracing::warn!(error = ?err, "accept failed"),
                }
            }
            _ = &mut shutdown => {
                tracing::info!("daemon shutting down");
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn handle_client(_ctx: Ctx, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    while reader.read_line(&mut line).await? > 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        let resp = match serde_json::from_str::<Request>(trimmed) {
            Ok(Request::Ping) => Response::Ok,
            Ok(other) => Response::Error {
                message: format!("not yet implemented: {other:?}"),
            },
            Err(err) => Response::Error {
                message: format!("malformed request: {err}"),
            },
        };
        let mut bytes = serde_json::to_vec(&resp)?;
        bytes.push(b'\n');
        write.write_all(&bytes).await?;
        line.clear();
    }
    Ok(())
}
