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
    #[serde(default)]
    demo_token: String,
    #[serde(default)]
    demo_upload_url: String,
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
    if !enrolled.demo_token.is_empty() {
        // Persisted (not just printed) so configuration management can wire
        // MatchZy idempotently on any later run — the demo token is only
        // ever returned by this one enroll response.
        write(
            "matchzy_portal.cfg",
            &format!(
                "// Written by portal-server-agent enroll. Server-scoped demo\n\
                 // upload settings for csgo/cfg/MatchZy/config.cfg — do NOT\n\
                 // put these in per-match cvars.\n\
                 matchzy_demo_upload_url \"{}\"\n\
                 matchzy_demo_upload_header_key \"Authorization\"\n\
                 matchzy_demo_upload_header_value \"Bearer {}\"\n",
                enrolled.demo_upload_url, enrolled.demo_token
            ),
            0o640,
        )?;
    }
    chown_to_service_user(dir);

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
    if !enrolled.demo_token.is_empty() {
        println!();
        println!("Add to csgo/cfg/MatchZy/config.cfg (server-scoped demo upload, survives");
        println!("cvar restores - do NOT put these in per-match cvars):");
        println!("  matchzy_demo_upload_url \"{}\"", enrolled.demo_upload_url);
        println!("  matchzy_demo_upload_header_key \"Authorization\"");
        println!(
            "  matchzy_demo_upload_header_value \"Bearer {}\"",
            enrolled.demo_token
        );
        println!();
        println!("(Also written to {dir}/matchzy_portal.cfg for config management.)");
    }
    Ok(())
}

/// Hand the material to the service user. `enroll` is documented as run
/// under sudo (the README), so everything lands root-owned — but the unit
/// runs as the static `portal-agent` user the deb postinst creates. Without
/// this the service can never read its own client key. Best-effort: on a
/// host without the deb (dev box, container) the user doesn't exist and we
/// silently skip; a failed chown warns with the manual command.
fn chown_to_service_user(dir: &str) {
    const USER: &str = "portal-agent";
    let user_exists = std::process::Command::new("getent")
        .args(["passwd", USER])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !user_exists {
        return;
    }
    let spec = format!("{USER}:{USER}");
    let ok = std::process::Command::new("chown")
        .args(["-R", &spec, dir])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        // Group needs to traverse the dir; the 0600 on client.key stands.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750));
        }
        println!("Ownership of {dir} handed to {USER} (the service user)");
    } else {
        eprintln!("warning: could not chown {dir} — run: chown -R {spec} {dir}");
    }
}

/// Renew the certificate over the established mTLS channel (§5.3 step 4):
/// authenticates with the CURRENT cert, submits a fresh CSR from a new
/// keypair, and atomically swaps the material on success.
pub async fn renew(agents_base_url: &str, dir: &str) -> Result<()> {
    let dir_path = Path::new(dir);
    let cert_pem = std::fs::read(dir_path.join("client.pem")).context("reading client.pem")?;
    let key_pem = std::fs::read(dir_path.join("client.key")).context("reading client.key")?;
    let ca_pem = std::fs::read(dir_path.join("portal-ca.pem")).context("reading portal-ca.pem")?;

    // reqwest identity: key + cert concatenated.
    let mut identity_pem = key_pem.clone();
    identity_pem.extend_from_slice(&cert_pem);
    let identity =
        reqwest::Identity::from_pem(&identity_pem).context("building client identity")?;
    let ca_cert = reqwest::Certificate::from_pem(&ca_pem).context("parsing portal CA")?;

    let new_key = rcgen::KeyPair::generate().context("generating new keypair")?;
    let csr_pem = rcgen::CertificateParams::default()
        .serialize_request(&new_key)
        .context("building CSR")?
        .pem()
        .context("encoding CSR")?;

    let url = format!(
        "{}/v1/gameserver/renew",
        agents_base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .identity(identity)
        .add_root_certificate(ca_cert)
        .build()
        .context("building https client")?;
    let response = client
        .post(&url)
        .json(&serde_json::json!({ "csr_pem": csr_pem }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("renewal rejected ({status}): {body}");
    }
    #[derive(Deserialize)]
    struct RenewResponse {
        certificate_pem: String,
        expires_at: String,
    }
    let renewed: RenewResponse = response.json().await.context("parsing renew response")?;

    std::fs::write(dir_path.join("client.key"), new_key.serialize_pem())
        .context("writing new client.key")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir_path.join("client.key"),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    std::fs::write(dir_path.join("client.pem"), &renewed.certificate_pem)
        .context("writing new client.pem")?;
    chown_to_service_user(dir);
    println!("Certificate renewed; valid until {}", renewed.expires_at);
    println!("Restart the agent to reconnect with the new certificate.");
    Ok(())
}
