use std::{io, net::SocketAddr, path::PathBuf, sync::Arc};

use serde_json::json;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let credentials = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let address = args
        .next()
        .map(|value| value.into_string().map_err(|_| usage()))
        .transpose()?
        .unwrap_or_else(|| "127.0.0.1:0".to_owned())
        .parse::<SocketAddr>()?;
    if args.next().is_some() || !address.ip().is_loopback() {
        return Err(usage().into());
    }
    let checkout = Arc::new(tempfile::Builder::new().prefix("cocodeop-m00-").tempdir()?);
    let listener = TcpListener::bind(address).await?;
    let address = listener.local_addr()?;
    let stop = CancellationToken::new();
    let router = office_compat_probe::router(credentials, checkout.clone(), address, stop.clone())?;
    println!(
        "{}",
        json!({"url": format!("http://{address}/mcp"), "fixture_root": checkout.path()})
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.cancel();
        })
        .await?;
    Ok(())
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: office-compat-probe <credential-hashes.json> [loopback-ip:port]",
    )
}
