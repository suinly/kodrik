pub mod ask;
pub mod connect;
pub mod gateway;
pub mod invites;
pub mod request_restart;
pub mod tui;
pub mod web;

#[doc(hidden)]
pub fn remote_supervisor_worker() -> anyhow::Result<()> {
    crate::remote::supervisor::worker()
}

pub(crate) async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! { _=tokio::signal::ctrl_c()=>"Ctrl-C", _=term.recv()=>"SIGTERM" }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

pub mod runs;
