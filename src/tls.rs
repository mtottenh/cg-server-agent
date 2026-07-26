//! rustls client configuration: client certificate (portal-CA-signed) +
//! server verification against the public webpki roots AND the portal CA.
//!
//! The server side of the trust is deliberately NOT pinned to the portal
//! CA alone: the agents vhost terminates TLS with an ACME (Let's Encrypt)
//! certificate — public DNS identity — while the mTLS guarantee lives in
//! the OTHER direction (Caddy verifying our client cert against the
//! portal CA). Pinning only the portal CA made every connection fail.

use anyhow::{Context, Result, bail};
use std::sync::Arc;

pub fn client_config(
    cert_path: &str,
    key_path: &str,
    ca_path: &str,
) -> Result<Arc<rustls::ClientConfig>> {
    let ca_pem = std::fs::read(ca_path).with_context(|| format!("reading {ca_path}"))?;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    for cert in rustls_pemfile::certs(&mut ca_pem.as_slice()) {
        roots
            .add(cert.context("parsing CA certificate")?)
            .context("adding CA to root store")?;
    }
    if roots.is_empty() {
        bail!("no certificates found in {ca_path}");
    }

    let cert_pem = std::fs::read(cert_path).with_context(|| format!("reading {cert_path}"))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<_, _>>()
        .context("parsing client certificate")?;
    if certs.is_empty() {
        bail!("no certificates found in {cert_path}");
    }

    let key_pem = std::fs::read(key_path).with_context(|| format!("reading {key_path}"))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .context("parsing client key")?
        .with_context(|| format!("no private key found in {key_path}"))?;

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .context("building TLS config with client certificate")?;
    Ok(Arc::new(config))
}
