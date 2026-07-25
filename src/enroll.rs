//! One-time enrollment: keypair + CSR → signed client certificate.
//!
//! Design: docs/matchzy-integration.md §5.3. The private key is generated
//! locally and never leaves this host; the portal signs the CSR and binds
//! the certificate CN to the server's UUID regardless of what we request.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct EnrollResponse {
    server_id: String,
    server_name: String,
    certificate_pem: String,
    ca_certificate_pem: String,
    expires_at: String,
}

pub async fn enroll(portal_url: &str, token: &str, dir: &str) -> Result<()> {
    let key = rcgen::KeyPair::generate().context("generating keypair")?;
    let csr_pem = rcgen::CertificateParams::default()
        .serialize_request(&key)
        .context("building CSR")?
        .pem()
        .context("encoding CSR")?;

    let url = format!("{}/v1/gameserver/enroll", portal_url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "enrollment_token": token, "csr_pem": csr_pem }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("enrollment rejected ({status}): {body}");
    }
    let enrolled: EnrollResponse = response.json().await.context("parsing enroll response")?;

    std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    let write = |name: &str, contents: &str, mode: u32| -> Result<()> {
        let path = Path::new(dir).join(name);
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    };
    write("client.key", &key.serialize_pem(), 0o600)?;
    write("client.pem", &enrolled.certificate_pem, 0o644)?;
    write("portal-ca.pem", &enrolled.ca_certificate_pem, 0o644)?;

    println!(
        "Enrolled as server {} ({})",
        enrolled.server_name, enrolled.server_id
    );
    println!("Certificate valid until {}", enrolled.expires_at);
    println!("Material written to {dir}/ (client.key, client.pem, portal-ca.pem)");
    println!();
    println!("Set in /etc/portal/portal-server-agent.env:");
    println!("  PORTAL_AGENT_URL=wss://agents.<portal-domain>/v1/gameserver/agent/ws");
    println!("  PORTAL_AGENT_CERT={dir}/client.pem");
    println!("  PORTAL_AGENT_KEY={dir}/client.key");
    println!("  PORTAL_AGENT_CA={dir}/portal-ca.pem");
    Ok(())
}
