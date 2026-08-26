use std::net::SocketAddr;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about = "Public rendezvous bridge for Super-Herdr")]
struct Options {
    /// Plain HTTP origin address. TLS terminates at Cloudflare Tunnel or a proxy.
    #[arg(long, default_value = "127.0.0.1:8789")]
    address: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    super_herdr_bridge::serve(options.address).await
}
