// SPDX-License-Identifier: MIT
//! Native per-user agent sessions and tool harness. Model execution stays in
//! enzimos-inference-service and provider routing in enzimos-ai-gateway; the
//! shell owns presentation.
mod desktop;
pub mod http_mcp;
mod local;
pub mod mcp;
pub mod provider;
pub mod runtime;
mod screenshot;
pub mod service;
pub mod skills;
pub mod tools;

/// Internal refusal; callers only see generic codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

pub type Result<T> = std::result::Result<T, ToolError>;

pub fn fail<T>(message: &str) -> Result<T> {
    Err(ToolError(message.into()))
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(data))
}

#[allow(unsafe_code, reason = "Linux syscall ABI (getuid).")]
pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// TLS with the system trust store, as Python's default SSL context.
pub fn tls_connector() -> Result<tokio_rustls::TlsConnector> {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for certificate in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(certificate);
    }
    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| ToolError("TLS unavailable".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(std::sync::Arc::new(
        config,
    )))
}
