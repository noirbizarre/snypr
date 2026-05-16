//! Helpers for handing values between the tokio runtime and the GLib main context.

use std::future::Future;

use glib::MainContext;

/// Spawn `fut` on the supplied tokio runtime; deliver the result to `cb` on the GLib main context.
pub fn tokio_to_glib<T, F, Cb>(handle: &tokio::runtime::Handle, fut: F, cb: Cb)
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
    Cb: FnOnce(T) + 'static,
{
    let (tx, rx) = async_channel::bounded::<T>(1);
    handle.spawn(async move {
        let _ = tx.send(fut.await).await;
    });
    MainContext::default().spawn_local(async move {
        if let Ok(value) = rx.recv().await {
            cb(value);
        }
    });
}
