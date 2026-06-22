use std::sync::Arc;

use lru::LruCache;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Manages a root CA and generates per-domain TLS certificates for MITM.
pub struct CertificateAuthority {
    ca_cert: Certificate,
    ca_key: KeyPair,
    cache: LruCache<String, Arc<ServerConfig>>,
}

impl CertificateAuthority {
    /// Generate a new self-signed root CA.
    pub fn new() -> anyhow::Result<Self> {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        params.key_usages.push(KeyUsagePurpose::CrlSign);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "Shuru Proxy CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Shuru");

        let key = KeyPair::generate()?;
        let cert = params.self_signed(&key)?;

        Ok(CertificateAuthority {
            ca_cert: cert,
            ca_key: key,
            cache: LruCache::new(std::num::NonZeroUsize::new(256).unwrap()),
        })
    }

    /// Get the CA certificate in PEM format (for injecting into the guest trust store).
    pub fn ca_cert_pem(&self) -> Vec<u8> {
        self.ca_cert.pem().into_bytes()
    }

    /// Get or create a TLS ServerConfig for the given domain.
    pub fn server_config_for_domain(&mut self, domain: &str) -> anyhow::Result<Arc<ServerConfig>> {
        if let Some(config) = self.cache.get(domain) {
            return Ok(config.clone());
        }

        let config = self.generate_server_config(domain)?;
        let config = Arc::new(config);
        self.cache.put(domain.to_string(), config.clone());
        Ok(config)
    }

    /// Get a TlsAcceptor for the given domain.
    pub fn acceptor_for_domain(&mut self, domain: &str) -> anyhow::Result<TlsAcceptor> {
        let config = self.server_config_for_domain(domain)?;
        Ok(TlsAcceptor::from(config))
    }

    fn generate_server_config(&self, domain: &str) -> anyhow::Result<ServerConfig> {
        let mut params = CertificateParams::new(vec![domain.to_string()])?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, domain);

        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.ca_cert, &self.ca_key)?;

        let cert_der = CertificateDer::from(cert.der().to_vec());
        let ca_cert_der = CertificateDer::from(self.ca_cert.der().to_vec());
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der, ca_cert_der], key_der)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_crypto() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    #[test]
    fn test_server_config_alpn_h1_only() {
        init_crypto();
        let mut ca = CertificateAuthority::new().unwrap();
        let config = ca.server_config_for_domain("example.com").unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn test_server_config_cached() {
        init_crypto();
        let mut ca = CertificateAuthority::new().unwrap();
        let c1 = ca.server_config_for_domain("example.com").unwrap();
        let c2 = ca.server_config_for_domain("example.com").unwrap();
        assert!(Arc::ptr_eq(&c1, &c2));
    }
}
