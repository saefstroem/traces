use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::error::Error;
use crate::result::Result;

/// Represents a Certificate Authority (CA) used for generating and signing certificates.
pub struct Ca {
    pub cert_path: PathBuf,
    issuer: Issuer<'static, KeyPair>,
}

impl Ca {
    /// Loads an existing CA from the specified directory or creates a new one if it doesn't exist.
    /// Returns a `Ca` instance containing the certificate path and issuer information.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        // Determine the paths for the CA certificate and key within the specified directory.
        let (cert_path, key_path) = (dir.join("ca.crt"), dir.join("ca.key"));
        // Helper closure to create an error with the path context.
        let at = |path: &Path| {
            let path = path.to_path_buf();
            move |source| Error::File { path, source }
        };
        // If either the certificate or key does not exist, create them.
        if !cert_path.exists() || !key_path.exists() {
            // Create the directory for the CA if it doesn't exist.
            DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(at(dir))?;
            // Generate a new key pair for the CA.
            let key = KeyPair::generate()?;
            // Set up the certificate parameters for the CA.
            let mut params = CertificateParams::default();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            params.distinguished_name.push(DnType::CommonName, "traces-ca");
            // Generate a self-signed certificate for the CA.
            let cert = params.self_signed(&key)?;
            // Write the key to the key file with appropriate permissions.
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&key_path)
                .and_then(|mut file| file.write_all(key.serialize_pem().as_bytes()))
                .map_err(at(&key_path))?;
            // Write the certificate to the certificate file.
            fs::write(&cert_path, cert.pem()).map_err(at(&cert_path))?;
        }
        // Load the existing key and certificate for the CA.
        let key = KeyPair::from_pem(&fs::read_to_string(&key_path).map_err(at(&key_path))?)?;
        let issuer = Issuer::from_ca_cert_pem(&fs::read_to_string(&cert_path).map_err(at(&cert_path))?, key)?;
        // Return the CA instance with the certificate path and issuer.
        Ok(Self { cert_path, issuer })
    }

    /// Generates a server configuration for the given host and ALPN protocols using the CA to sign the server certificate.
    pub fn server_config(&self, host: &str, alpn: &[u8]) -> Result<ServerConfig> {
        // Generate a new key pair for the server certificate.
        let key = KeyPair::generate()?;
        // Set up the certificate parameters for the server certificate.
        let mut params = CertificateParams::new(vec![host.to_string()])?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        // Sign the server certificate with the CA issuer.
        let cert = params.signed_by(&key, &self.issuer)?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut config =
            ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert.der().clone()], key)?;
        config.alpn_protocols = vec![alpn.to_vec()];
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::{ClientConfig, RootCertStore};
    use tempfile::tempdir;
    use tokio::io::duplex;
    use tokio::join;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::Ca;

    #[tokio::test]
    async fn leaf_certs_chain_to_the_saved_ca_and_negotiate_h2_or_http1() {
        let dir = tempdir().unwrap();
        let ca_dir = dir.path().join("ca");
        let first = Ca::load_or_create(&ca_dir).unwrap();
        let cert_path = first.cert_path.clone();
        let mode = |name: &str| fs::metadata(ca_dir.join(name)).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(""), mode("ca.key")), (0o700, 0o600));
        let connector = |alpn: &[&[u8]]| {
            let mut roots = RootCertStore::empty();
            roots.add(CertificateDer::from_pem_file(&cert_path).unwrap()).unwrap();
            let mut config = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
            config.alpn_protocols = alpn.iter().map(|proto| proto.to_vec()).collect();
            TlsConnector::from(Arc::new(config))
        };
        let cases = [
            (vec![b"h2".as_slice(), b"http/1.1".as_slice()], b"h2".as_slice()),
            (vec![b"http/1.1".as_slice()], b"http/1.1".as_slice()),
        ];
        for (ca, (offered, expected)) in [first, Ca::load_or_create(&ca_dir).unwrap()].into_iter().zip(cases) {
            let (client_io, server_io) = duplex(1 << 16);
            let server = TlsAcceptor::from(Arc::new(ca.server_config("api.example.com", expected).unwrap()));
            let name = ServerName::try_from("api.example.com").unwrap();
            let (accepted, connected) = join!(server.accept(server_io), connector(&offered).connect(name, client_io));
            assert_eq!(accepted.unwrap().get_ref().1.alpn_protocol(), Some(expected));
            assert_eq!(connected.unwrap().get_ref().1.alpn_protocol(), Some(expected));
        }
    }
}
