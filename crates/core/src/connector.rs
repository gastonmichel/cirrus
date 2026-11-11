//! Native Cloud SQL connector — the same protocol `cloud-sql-proxy` speaks:
//!
//! 1. generate an RSA key pair (once per process),
//! 2. ask the SQL Admin API for an ephemeral client certificate for that key,
//!    bound to the caller's IAM identity,
//! 3. open a mutual-TLS connection to the instance's server-side proxy on port 3307,
//!    trusting only the instance's server CA.
//!
//! The resulting stream carries the plain database protocol.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::{RsaPrivateKey, RsaPublicKey};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_cert_signed_by_trust_anchor, verify_server_name};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, OnceCell};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::admin::{self, ConnectSettings};
use crate::gcloud;

const SERVER_PROXY_PORT: u16 = 3307;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Refresh certificates this long before they expire.
const REFRESH_MARGIN_SECS: i64 = 5 * 60;

struct KeyPair {
    private_pkcs8_der: Vec<u8>,
    public_pem: String,
}

async fn key_pair() -> Result<&'static KeyPair> {
    static KEY: OnceCell<KeyPair> = OnceCell::const_new();
    KEY.get_or_try_init(|| async {
        tokio::task::spawn_blocking(|| -> Result<KeyPair> {
            let key = RsaPrivateKey::new(&mut rand::thread_rng(), 2048)?;
            Ok(KeyPair {
                private_pkcs8_der: key.to_pkcs8_der()?.as_bytes().to_vec(),
                public_pem: RsaPublicKey::from(&key).to_public_key_pem(LineEnding::LF)?,
            })
        })
        .await?
    })
    .await
}

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .clone()
}

/// TLS material for one instance, valid until `expires_at` (unix seconds).
struct Prepared {
    settings: ConnectSettings,
    config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
    expires_at: i64,
}

fn cache() -> &'static Mutex<HashMap<String, Arc<Prepared>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Prepared>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn prepared(project: &str, instance: &str) -> Result<Arc<Prepared>> {
    let key = format!("{project}:{instance}");
    let mut cache = cache().lock().await;
    if let Some(p) = cache.get(&key)
        && p.expires_at - chrono::Utc::now().timestamp() > REFRESH_MARGIN_SECS
    {
        return Ok(p.clone());
    }
    let p = Arc::new(prepare(project, instance).await?);
    cache.insert(key, p.clone());
    Ok(p)
}

async fn prepare(project: &str, instance: &str) -> Result<Prepared> {
    let keys = key_pair().await?;
    let token = gcloud::access_token().await?;
    let (settings, client_cert_pem) = tokio::try_join!(
        admin::connect_settings(project, instance),
        admin::generate_ephemeral_cert(project, instance, &keys.public_pem, &token),
    )?;

    let client_cert = CertificateDer::from_pem_slice(client_cert_pem.as_bytes())
        .context("invalid ephemeral certificate")?;
    let expires_at = x509_parser::parse_x509_certificate(&client_cert)
        .map_err(|e| anyhow!("invalid ephemeral certificate: {e}"))?
        .1
        .validity()
        .not_after
        .timestamp();

    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(settings.server_ca_cert.cert.as_bytes()) {
        roots.add(ca.context("invalid server CA certificate")?)?;
    }

    let (expected, server_name) = match (settings.uses_cas(), settings.dns_name.as_deref()) {
        (true, Some(dns)) if !dns.is_empty() => {
            let name = ServerName::try_from(dns.trim_end_matches('.').to_string())?;
            (ExpectedIdentity::DnsName(name.clone()), name)
        }
        _ => {
            let ip: std::net::IpAddr = settings.preferred_ip()?.parse()?;
            (
                ExpectedIdentity::CommonName(format!("{project}:{instance}")),
                ServerName::IpAddress(ip.into()),
            )
        }
    };

    let verifier = Arc::new(CloudSqlVerifier { roots, expected, provider: provider() });
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(keys.private_pkcs8_der.clone()));
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![client_cert.into_owned()], private_key)?;

    Ok(Prepared { settings, config: Arc::new(config), server_name, expires_at })
}

/// Opens an authenticated TLS stream to the instance's server-side proxy.
pub async fn connect(project: &str, instance: &str) -> Result<TlsStream<TcpStream>> {
    let p = prepared(project, instance).await?;
    let ip = p.settings.preferred_ip()?;
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, SERVER_PROXY_PORT)))
        .await
        .map_err(|_| anyhow!("timed out connecting to {ip}:{SERVER_PROXY_PORT} (is the IP reachable from this machine?)"))?
        .with_context(|| format!("failed to connect to {ip}:{SERVER_PROXY_PORT}"))?;
    tcp.set_nodelay(true)?;
    let tls = TlsConnector::from(p.config.clone())
        .connect(p.server_name.clone(), tcp)
        .await
        .context("TLS handshake with Cloud SQL failed")?;
    Ok(tls)
}

/// Forgets cached certificates for an instance (e.g. after an auth failure).
pub async fn invalidate(project: &str, instance: &str) {
    cache().lock().await.remove(&format!("{project}:{instance}"));
}

enum ExpectedIdentity {
    /// Legacy per-instance CA: the server certificate's CN is `project:instance`.
    CommonName(String),
    /// CAS-based CA: the server certificate carries the instance DNS name as a SAN.
    DnsName(ServerName<'static>),
}

struct CloudSqlVerifier {
    roots: RootCertStore,
    expected: ExpectedIdentity,
    provider: Arc<CryptoProvider>,
}

impl fmt::Debug for CloudSqlVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CloudSqlVerifier")
    }
}

impl ServerCertVerifier for CloudSqlVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        match &self.expected {
            ExpectedIdentity::DnsName(name) => verify_server_name(&cert, name)?,
            ExpectedIdentity::CommonName(expected) => {
                let cn = common_name(end_entity).map_err(|e| rustls::Error::General(e.to_string()))?;
                if &cn != expected {
                    return Err(rustls::Error::General(format!(
                        "server certificate is for {cn:?}, expected {expected:?}"
                    )));
                }
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn common_name(der: &[u8]) -> Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| anyhow!("bad certificate: {e}"))?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .ok_or_else(|| anyhow!("server certificate has no common name"))?;
    match cn.as_str() {
        Ok(s) => Ok(s.to_string()),
        Err(_) => bail!("server certificate common name is not a string"),
    }
}
