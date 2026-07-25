//! portal-server-agent — the game-server half of the portal's MatchZy
//! integration (docs/matchzy-integration.md §5).
//!
//! `enroll` exchanges a one-time token + locally generated CSR for a signed
//! client certificate. The default `run` mode maintains an outbound mTLS
//! WebSocket to the portal, executes match-setup commands via RCON on
//! localhost, and reports `get5_status` heartbeats.

mod enroll;
mod rcon;
mod run;
mod tls;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "portal-server-agent", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Portal agent WebSocket URL
    #[arg(long, env = "PORTAL_AGENT_URL", default_value = "")]
    url: String,

    /// Client certificate path (from enrollment)
    #[arg(long, env = "PORTAL_AGENT_CERT")]
    cert: Option<String>,

    /// Client key path (from enrollment)
    #[arg(long, env = "PORTAL_AGENT_KEY")]
    key: Option<String>,

    /// Portal CA certificate path (from enrollment)
    #[arg(long, env = "PORTAL_AGENT_CA")]
    ca: Option<String>,

    /// RCON address of the local CS2 server
    #[arg(long, env = "RCON_ADDR", default_value = "127.0.0.1:27015")]
    rcon_addr: String,

    /// RCON password of the local CS2 server
    #[arg(long, env = "RCON_PASSWORD", default_value = "")]
    rcon_password: String,

    /// Heartbeat interval in seconds
    #[arg(long, env = "PORTAL_AGENT_HEARTBEAT_SECS", default_value_t = 30)]
    heartbeat_secs: u64,

    /// DEV ONLY: authenticate with X-Dev-Server-Id instead of mTLS
    #[arg(long, env = "PORTAL_AGENT_DEV_SERVER_ID")]
    dev_server_id: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Exchange a one-time enrollment token for a client certificate
    Enroll {
        /// One-time enrollment token (minted by a portal admin)
        #[arg(long)]
        token: String,
        /// Portal base URL (e.g. https://portal.example.com)
        #[arg(long)]
        url: String,
        /// Directory to write client.key / client.pem / portal-ca.pem
        #[arg(long, default_value = "/etc/portal/agent")]
        dir: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // rustls 0.23 needs a process-level crypto provider; reqwest and
    // tokio-tungstenite may link different defaults, so pick one explicitly.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Enroll { token, url, dir }) => enroll::enroll(&url, &token, &dir).await,
        None => {
            anyhow::ensure!(!cli.url.is_empty(), "PORTAL_AGENT_URL is required");
            anyhow::ensure!(
                !cli.rcon_password.is_empty(),
                "RCON_PASSWORD is required (start the CS2 server with -usercon)"
            );
            run::run(run::RunConfig {
                portal_url: cli.url,
                cert_path: cli.cert,
                key_path: cli.key,
                ca_path: cli.ca,
                rcon_addr: cli.rcon_addr,
                rcon_password: cli.rcon_password,
                heartbeat_secs: cli.heartbeat_secs,
                dev_server_id: cli.dev_server_id,
            })
            .await
        }
    }
}
