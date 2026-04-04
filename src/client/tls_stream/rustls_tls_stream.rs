use crate::{
    client::{config::Config, TrustConfig},
    error::IoErrorKind,
    Error,
};
use futures_util::io::{AsyncRead, AsyncWrite};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme,
};
use std::{
    fs, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio_rustls::TlsConnector;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::{event, Level};

impl From<RustlsError> for Error {
    fn from(e: RustlsError) -> Self {
        crate::Error::Tls(e.to_string())
    }
}

pub(crate) struct TlsStream<S: AsyncRead + AsyncWrite + Unpin + Send>(
    Compat<tokio_rustls::client::TlsStream<Compat<S>>>,
);

/// Returns the process-wide CryptoProvider, or falls back to aws-lc-rs if none
/// is installed. Callers may install their own provider before opening a
/// connection; tiberius will respect it.
fn get_crypto_provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

#[derive(Debug)]
struct NoCertVerifier(Arc<CryptoProvider>);

impl ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn get_server_name(config: &Config) -> crate::Result<ServerName<'static>> {
    match (
        ServerName::try_from(config.get_host().to_owned()),
        &config.trust,
    ) {
        (Ok(sn), _) => Ok(sn),
        (Err(_), TrustConfig::TrustAll) => {
            Ok(ServerName::try_from("placeholder.domain.com".to_owned()).unwrap())
        }
        (Err(e), _) => Err(crate::Error::Tls(e.to_string())),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> TlsStream<S> {
    pub(super) async fn new(config: &Config, stream: S) -> crate::Result<Self> {
        event!(Level::INFO, "Performing a TLS handshake");

        let provider = get_crypto_provider();

        let client_config = match &config.trust {
            TrustConfig::CaCertificatePem(cert) => {
                let pem_cert = rustls_pemfile::certs(&mut cert.as_bytes())?;
                if pem_cert.len() != 1 {
                    return Err(crate::Error::Io {
                        kind: IoErrorKind::InvalidInput,
                        message: format!("Proided PEM cannot contain more than 1 cert"),
                    });
                }

                let cert = Certificate(pem_cert.into_iter().next().unwrap());

                let mut cert_store = RootCertStore::empty();
                cert_store.add(&cert)?;
                builder
                    .with_root_certificates(cert_store)
                    .with_no_client_auth()
            }
            TrustConfig::CaCertificateLocation(path) => {
                if let Ok(buf) = fs::read(path) {
                    let cert = match path.extension() {
                        Some(ext)
                            if ext.to_ascii_lowercase() == "pem"
                                || ext.to_ascii_lowercase() == "crt" =>
                        {
                            let mut pem_certs: Vec<CertificateDer<'static>> =
                                rustls_pemfile::certs(&mut buf.as_slice())
                                    .collect::<Result<Vec<_>, _>>()?;
                            if pem_certs.len() != 1 {
                                return Err(crate::Error::Io {
                                    kind: IoErrorKind::InvalidInput,
                                    message: format!(
                                        "Certificate file {} contain 0 or more than 1 certs",
                                        path.to_string_lossy()
                                    ),
                                });
                            }

                            pem_certs.remove(0)
                        }
                        Some(ext) if ext.to_ascii_lowercase() == "der" => {
                            CertificateDer::from(buf)
                        }
                        Some(_) | None => {
                            return Err(crate::Error::Io {
                                kind: IoErrorKind::InvalidInput,
                                message: "Provided CA certificate with unsupported file-extension! Supported types are pem, crt and der.".to_string(),
                            })
                        }
                    };
                    let mut cert_store = RootCertStore::empty();
                    cert_store.add(cert)?;
                    ClientConfig::builder_with_provider(provider)
                        .with_safe_default_protocol_versions()?
                        .with_root_certificates(cert_store)
                        .with_no_client_auth()
                } else {
                    return Err(Error::Io {
                        kind: IoErrorKind::InvalidData,
                        message: "Could not read provided CA certificate!".to_string(),
                    });
                }
            }
            TrustConfig::TrustAll => {
                event!(
                    Level::WARN,
                    "Trusting the server certificate without validation."
                );
                let mut config = ClientConfig::builder_with_provider(provider.clone())
                    .with_safe_default_protocol_versions()?
                    .with_root_certificates(RootCertStore::empty())
                    .with_no_client_auth();
                config
                    .dangerous()
                    .set_certificate_verifier(Arc::new(NoCertVerifier(provider)));
                config
            }
            TrustConfig::Default => {
                event!(Level::INFO, "Using default trust configuration.");
                let roots = load_native_roots();
                ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()?
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            }
        };

        let connector = TlsConnector::from(Arc::new(client_config));

        let tls_stream = connector
            .connect(get_server_name(config)?, stream.compat())
            .await?;

        Ok(TlsStream(tls_stream.compat()))
    }

    pub(crate) fn get_mut(&mut self) -> &mut S {
        self.0.get_mut().get_mut().0.get_mut()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> AsyncRead for TlsStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let inner = Pin::get_mut(self);
        Pin::new(&mut inner.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> AsyncWrite for TlsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let inner = Pin::get_mut(self);
        Pin::new(&mut inner.0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = Pin::get_mut(self);
        Pin::new(&mut inner.0).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = Pin::get_mut(self);
        Pin::new(&mut inner.0).poll_close(cx)
    }
}

fn load_native_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    let mut valid_count = 0;
    let mut invalid_count = 0;

    for cert in rustls_native_certs::load_native_certs().expect("could not load platform certs") {
        match roots.add(cert) {
            Ok(_) => valid_count += 1,
            Err(err) => {
                tracing::event!(Level::DEBUG, "certificate parsing failed: {:?}", err);
                invalid_count += 1
            }
        }
    }
    tracing::event!(
        Level::TRACE,
        "load_native_roots processed {} valid and {} invalid certs",
        valid_count,
        invalid_count
    );
    assert!(!roots.is_empty(), "no CA certificates found");

    roots
}
