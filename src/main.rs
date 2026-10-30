use std::net::SocketAddr;
use std::sync::Arc;

use docstore::{api, store::Store};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
    let listen: SocketAddr = std::env::var("LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9200".into())
        .parse()?;

    let started = std::time::Instant::now();
    let store = Store::open(&data_dir)?;
    let (indices, docs) = store.with_indices(None, |s| {
        (s.len(), s.iter().map(|(_, d)| d.len()).sum::<usize>())
    })?;
    tracing::info!(
        data_dir,
        indices,
        docs,
        load_ms = started.elapsed().as_millis() as u64,
        "store loaded"
    );

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "listening");
    axum::serve(listener, api::router(Arc::new(store)))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}
