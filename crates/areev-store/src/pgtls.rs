//! TLS for the Postgres transport — the `sslmode=` / `sslrootcert=` legs of
//! the DSN, and the one `connect` every Postgres session goes through.
//!
//! Two parameters in the DSN are ours to interpret rather than the driver's:
//!
//! - **`sslmode`** — tokio-postgres understands only `disable`/`prefer`/
//!   `require`, and `require` to it means "a TLS stack must be present", not
//!   "the certificate must check out". libpq's ladder has five rungs and the
//!   top two (`verify-ca`, `verify-full`) are the ones an operator reaches for
//!   on a managed database, so we parse the whole ladder and map it onto a
//!   driver mode plus a certificate verifier.
//! - **`sslrootcert`** — tokio-postgres rejects it outright as an unknown
//!   option, so leaving it in the DSN fails the connection before TLS is even
//!   considered. Both are split out here and the remainder is handed to the
//!   driver.
//!
//! The verification semantics are **libpq's, deliberately**, because that is
//! what the DSN in an operator's secret manager already means:
//!
//! | `sslmode` | encrypted | chain checked | hostname checked |
//! |---|---|---|---|
//! | `disable` | no | — | — |
//! | `prefer` (default) | if the server offers it | no | no |
//! | `require` | yes | no | no |
//! | `verify-ca` | yes | yes | no |
//! | `verify-full` | yes | yes | yes |
//!
//! `require` not verifying looks wrong until you try it: AWS RDS signs with
//! its own `rds-ca-*` roots, which are not in any public trust store, so a
//! `require` that quietly verified against Mozilla's roots would fail every
//! stock RDS DSN. An operator who wants the guarantee asks for `verify-full`
//! and points `sslrootcert` at the provider bundle — and that is the mode the
//! deployment docs recommend. What this module will never do is *downgrade*:
//! a build without `postgres-tls` refuses `require` and above by name
//! (`STO-E003`) instead of connecting in plaintext.

use areev_core::error::{AreevError, Result};

/// libpq's `sslmode` ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    fn parse(v: &str) -> Result<Self> {
        match v {
            "disable" => Ok(Self::Disable),
            "allow" | "prefer" => Ok(Self::Prefer),
            "require" => Ok(Self::Require),
            "verify-ca" => Ok(Self::VerifyCa),
            "verify-full" => Ok(Self::VerifyFull),
            other => Err(AreevError::Validation(format!(
                "unknown sslmode {other:?} — expected one of disable, allow, prefer, \
                 require, verify-ca, verify-full"
            ))),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    /// Does this rung make TLS mandatory? These are exactly the rungs a build
    /// without `postgres-tls` must refuse rather than serve in the clear —
    /// which is the only build that has to ask.
    #[cfg(not(feature = "postgres-tls"))]
    pub(crate) fn demands_tls(self) -> bool {
        matches!(self, Self::Require | Self::VerifyCa | Self::VerifyFull)
    }

    /// Does this rung check the server's certificate chain? The `verify-*`
    /// pair, and only it — see the ladder in the module header.
    #[cfg(feature = "postgres-tls")]
    fn verifies_chain(self) -> bool {
        matches!(self, Self::VerifyCa | Self::VerifyFull)
    }
}

/// A DSN with our two TLS parameters split out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SslRequest {
    /// The connection string with `sslmode`/`sslrootcert` removed — what the
    /// driver's own parser sees.
    pub(crate) dsn: String,
    pub(crate) mode: SslMode,
    /// A PEM bundle path, or `None`/`"system"` for the compiled-in Mozilla
    /// roots. Taken literally: a query value may contain `/` and `.`
    /// unencoded, and percent-decoding here would corrupt a path that
    /// legitimately contains `%`.
    pub(crate) root_cert: Option<String>,
}

impl SslRequest {
    /// Split `sslmode`/`sslrootcert` out of a `postgres://…` URL.
    ///
    /// Mirrors [`crate::pg::split_schema_url`]'s treatment of `?schema=`:
    /// query form only (which is the only form that function admits anyway),
    /// unrecognized pairs passed through untouched.
    pub(crate) fn split(url: &str) -> Result<Self> {
        let Some((base, query)) = url.split_once('?') else {
            return Ok(Self { dsn: url.to_string(), mode: SslMode::Prefer, root_cert: None });
        };
        let mut mode = SslMode::Prefer;
        let mut root_cert = None;
        let mut rest: Vec<&str> = Vec::new();
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("sslmode", v)) => mode = SslMode::parse(v)?,
                Some(("sslrootcert", v)) if !v.is_empty() => root_cert = Some(v.to_string()),
                _ => rest.push(pair),
            }
        }
        let dsn = if rest.is_empty() {
            base.to_string()
        } else {
            format!("{base}?{}", rest.join("&"))
        };
        Ok(Self { dsn, mode, root_cert })
    }
}

/// The refusal a build without `postgres-tls` gives a DSN that asks for
/// encryption. Named, so it reads as "this build cannot" rather than as a
/// typo — and so it can never be mistaken for a connection that succeeded.
#[cfg(not(feature = "postgres-tls"))]
fn compiled_out(mode: SslMode) -> AreevError {
    AreevError::TlsUnavailable(format!(
        "the DSN asks for sslmode={} but Postgres TLS was not compiled into this build \
         (cargo feature \"postgres-tls\"). Refusing rather than connecting in plaintext. \
         Rebuild with the feature, or terminate TLS in a local proxy (Cloud SQL Auth Proxy, \
         PgBouncer with a TLS upstream) and point the DSN at it with sslmode=disable",
        mode.as_str()
    ))
}

/// Open one Postgres session, honoring the DSN's TLS request.
///
/// Returns just the `Client`: the connection future differs in type between
/// the plaintext and TLS arms, and every caller does the same thing with it
/// (spawn it on this runtime so the client can make progress), so it is
/// spawned here rather than handed back.
pub(crate) fn connect(
    rt: &tokio::runtime::Runtime,
    url: &str,
) -> Result<tokio_postgres::Client> {
    let req = SslRequest::split(url)?;
    let mut cfg: tokio_postgres::Config = req.dsn.parse().map_err(crate::pg::pg_err)?;
    // `sslmode` was stripped above, so the parsed config carries the driver
    // default; set the mode we actually resolved.
    cfg.ssl_mode(match req.mode {
        SslMode::Disable => tokio_postgres::config::SslMode::Disable,
        SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
        // The driver's `Require` only means "do not fall back to plaintext".
        // Everything about *checking* the peer is the verifier's job below.
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
            tokio_postgres::config::SslMode::Require
        }
    });

    #[cfg(not(feature = "postgres-tls"))]
    {
        if req.mode.demands_tls() {
            return Err(compiled_out(req.mode));
        }
        if req.root_cert.is_some() {
            return Err(AreevError::Validation(
                "the DSN sets sslrootcert but Postgres TLS was not compiled into this build \
                 (cargo feature \"postgres-tls\")"
                    .into(),
            ));
        }
        spawn_plaintext(rt, &cfg)
    }

    #[cfg(feature = "postgres-tls")]
    {
        if req.mode == SslMode::Disable {
            if req.root_cert.is_some() {
                return Err(AreevError::Validation(
                    "the DSN sets both sslmode=disable and sslrootcert — the connection would \
                     be plaintext and the root bundle unused; drop one"
                        .into(),
                ));
            }
            return spawn_plaintext(rt, &cfg);
        }
        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(rustls_config(&req)?);
        let (client, connection) = rt.block_on(cfg.connect(tls)).map_err(crate::pg::pg_err)?;
        rt.spawn(async move {
            let _ = connection.await;
        });
        Ok(client)
    }
}

fn spawn_plaintext(
    rt: &tokio::runtime::Runtime,
    cfg: &tokio_postgres::Config,
) -> Result<tokio_postgres::Client> {
    let (client, connection) = rt
        .block_on(cfg.connect(tokio_postgres::NoTls))
        .map_err(crate::pg::pg_err)?;
    rt.spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

#[cfg(feature = "postgres-tls")]
mod verify {
    use std::sync::Arc;

    use areev_core::error::{AreevError, Result};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{CertificateError, DigitallySignedStruct, Error as TlsError, SignatureScheme};

    use super::{SslMode, SslRequest};

    /// `require`/`prefer`: encrypt, check nothing. libpq's semantics — see the
    /// module header for why this rung exists rather than being folded into
    /// `verify-full`.
    #[derive(Debug)]
    pub(super) struct AcceptAny(Arc<CryptoProvider>);

    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<ServerCertVerified, TlsError> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    /// `verify-ca`: the chain must check out, the name need not.
    ///
    /// Wraps the real webpki verifier and forgives exactly one failure —
    /// the name mismatch — so the chain, expiry, and revocation legs stay
    /// intact. Written as "delegate, then forgive" rather than as a
    /// reimplementation for that reason.
    #[derive(Debug)]
    pub(super) struct AnyHostname(Arc<dyn ServerCertVerifier>);

    impl ServerCertVerifier for AnyHostname {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            intermediates: &[CertificateDer<'_>],
            server_name: &ServerName<'_>,
            ocsp: &[u8],
            now: UnixTime,
        ) -> std::result::Result<ServerCertVerified, TlsError> {
            match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
                Err(TlsError::InvalidCertificate(
                    CertificateError::NotValidForName
                    | CertificateError::NotValidForNameContext { .. },
                )) => Ok(ServerCertVerified::assertion()),
                other => other,
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
            self.0.verify_tls12_signature(message, cert, dss)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
            self.0.verify_tls13_signature(message, cert, dss)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.supported_verify_schemes()
        }
    }

    /// The trust anchors a verifying mode checks against: the caller's PEM
    /// bundle, else the compiled-in Mozilla roots.
    ///
    /// `sslrootcert=system` is libpq's spelling for "the platform store"; here
    /// it means the compiled-in bundle, which is the same promise a static
    /// binary can actually keep — no OS trust store is read.
    pub(super) fn roots(req: &SslRequest) -> Result<rustls::RootCertStore> {
        let mut store = rustls::RootCertStore::empty();
        match req.root_cert.as_deref() {
            None | Some("system") => {
                store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            Some(path) => {
                use rustls::pki_types::pem::PemObject;
                let certs = CertificateDer::pem_file_iter(path).map_err(|e| {
                    AreevError::Validation(format!("sslrootcert {path:?}: {e}"))
                })?;
                for cert in certs {
                    let cert = cert.map_err(|e| {
                        AreevError::Validation(format!("sslrootcert {path:?}: {e}"))
                    })?;
                    store
                        .add(cert)
                        .map_err(|e| AreevError::Validation(format!("sslrootcert {path:?}: {e}")))?;
                }
                // An empty-but-readable bundle is the dangerous case: every
                // chain would fail to verify, and an operator debugging that
                // deserves to be told the file is the problem.
                if store.is_empty() {
                    return Err(AreevError::Validation(format!(
                        "sslrootcert {path:?} contained no PEM certificates"
                    )));
                }
            }
        }
        Ok(store)
    }

    pub(super) fn verifier(
        req: &SslRequest,
        provider: &Arc<CryptoProvider>,
    ) -> Result<Arc<dyn ServerCertVerifier>> {
        if !req.mode.verifies_chain() {
            return Ok(Arc::new(AcceptAny(Arc::clone(provider))));
        }
        let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots(req)?),
            Arc::clone(provider),
        )
        .build()
        .map_err(|e| {
            AreevError::Validation(format!(
                "postgres TLS verifier for sslmode={}: {e}",
                req.mode.as_str()
            ))
        })?;
        Ok(match req.mode {
            SslMode::VerifyFull => webpki,
            _ => Arc::new(AnyHostname(webpki)),
        })
    }
}

#[cfg(feature = "postgres-tls")]
fn rustls_config(req: &SslRequest) -> Result<rustls::ClientConfig> {
    use std::sync::Arc;

    // An explicit provider rather than the process-wide default: this library
    // is linked beside other rustls users (ureq for the LLM providers,
    // areev-server's `tls`), and reaching for a process default would make our
    // connection depend on whether some other component installed one first.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = verify::verifier(req, &provider)?;
    Ok(rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| AreevError::Validation(format!("postgres TLS: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_our_two_parameters_and_passes_the_rest_through() {
        let r = SslRequest::split(
            "postgres://u:p@h:5432/db?sslmode=verify-full&application_name=areev\
             &sslrootcert=/etc/ssl/azure.pem",
        )
        .unwrap();
        assert_eq!(r.dsn, "postgres://u:p@h:5432/db?application_name=areev");
        assert_eq!(r.mode, SslMode::VerifyFull);
        assert_eq!(r.root_cert.as_deref(), Some("/etc/ssl/azure.pem"));
    }

    #[test]
    fn a_query_that_was_only_ours_leaves_no_dangling_question_mark() {
        let r = SslRequest::split("postgres://h/db?sslmode=require").unwrap();
        assert_eq!(r.dsn, "postgres://h/db");
        assert_eq!(r.mode, SslMode::Require);
    }

    /// The default is libpq's, not "off": every existing plaintext DSN keeps
    /// working, and nothing silently starts demanding TLS.
    #[test]
    fn no_sslmode_means_prefer() {
        assert_eq!(SslRequest::split("postgres://h/db").unwrap().mode, SslMode::Prefer);
        assert_eq!(
            SslRequest::split("postgres://h/db?application_name=x").unwrap().mode,
            SslMode::Prefer
        );
    }

    #[test]
    fn the_whole_libpq_ladder_parses() {
        for (s, want) in [
            ("disable", SslMode::Disable),
            ("allow", SslMode::Prefer),
            ("prefer", SslMode::Prefer),
            ("require", SslMode::Require),
            ("verify-ca", SslMode::VerifyCa),
            ("verify-full", SslMode::VerifyFull),
        ] {
            let r = SslRequest::split(&format!("postgres://h/db?sslmode={s}")).unwrap();
            assert_eq!(r.mode, want, "sslmode={s}");
        }
    }

    #[test]
    fn an_unknown_sslmode_is_refused_by_name() {
        let e = SslRequest::split("postgres://h/db?sslmode=verify_full").unwrap_err();
        assert_eq!(e.code(), "VAL-E001");
        assert!(e.to_string().contains("verify_full"), "{e}");
    }

    /// Only the three rungs that promise encryption are refusable — a build
    /// without TLS must keep serving plaintext DSNs exactly as before.
    #[cfg(not(feature = "postgres-tls"))]
    #[test]
    fn only_the_encrypting_rungs_demand_tls() {
        assert!(!SslMode::Disable.demands_tls());
        assert!(!SslMode::Prefer.demands_tls());
        assert!(SslMode::Require.demands_tls());
        assert!(SslMode::VerifyCa.demands_tls());
        assert!(SslMode::VerifyFull.demands_tls());
    }

    #[cfg(not(feature = "postgres-tls"))]
    #[test]
    fn the_compiled_out_refusal_names_the_feature_and_the_mode() {
        let e = compiled_out(SslMode::VerifyFull);
        assert_eq!(e.code(), "STO-E003");
        let msg = e.to_string();
        assert!(msg.contains("postgres-tls"), "{msg}");
        assert!(msg.contains("verify-full"), "{msg}");
        assert!(msg.contains("plaintext"), "{msg}");
    }

    /// The whole point of the issue: a DSN that asks for encryption must never
    /// come back as a plaintext connection.
    #[cfg(not(feature = "postgres-tls"))]
    #[test]
    fn a_build_without_tls_refuses_rather_than_downgrades() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for mode in ["require", "verify-ca", "verify-full"] {
            let e = connect(&rt, &format!("postgres://u@127.0.0.1:1/db?sslmode={mode}"))
                .expect_err("{mode} must be refused");
            assert_eq!(e.code(), "STO-E003", "{mode}: {e}");
        }
    }

    #[cfg(feature = "postgres-tls")]
    #[test]
    fn a_root_bundle_that_is_not_a_bundle_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.pem");
        std::fs::write(&path, b"not a certificate\n").unwrap();
        let req = SslRequest::split(&format!(
            "postgres://h/db?sslmode=verify-full&sslrootcert={}",
            path.display()
        ))
        .unwrap();
        let e = rustls_config(&req).unwrap_err();
        assert!(e.to_string().contains("sslrootcert"), "{e}");
    }

    #[cfg(feature = "postgres-tls")]
    #[test]
    fn every_encrypting_mode_builds_a_client_config() {
        for mode in ["prefer", "require", "verify-ca", "verify-full"] {
            let req = SslRequest::split(&format!("postgres://h/db?sslmode={mode}")).unwrap();
            rustls_config(&req).unwrap_or_else(|e| panic!("sslmode={mode}: {e}"));
        }
    }
}

/// A real TLS handshake against a fake Postgres, because a `ClientConfig` that
/// builds proves nothing about what it accepts.
///
/// The server here speaks exactly the four bytes of Postgres that matter for
/// this feature — read the 8-byte SSLRequest, answer `S`, hand the socket to
/// rustls — and then hangs up. That is enough to separate the two outcomes the
/// whole issue turns on: did the CLIENT accept this certificate, or refuse it?
/// A run that gets past the handshake fails later with a protocol error, which
/// is how each test tells "encrypted" from "verified".
#[cfg(all(test, feature = "postgres-tls"))]
mod handshake_tests {
    use std::io::{Read, Write};
    use std::sync::mpsc;

    use super::*;

    struct Pki {
        ca_pem: String,
        leaf_der: rustls::pki_types::CertificateDer<'static>,
        leaf_key: rustls::pki_types::PrivateKeyDer<'static>,
    }

    /// A throwaway CA and a leaf signed by it, naming `sans` — the shape a
    /// managed provider ships (Azure's DigiCert bundle, RDS's `rds-ca-*`): a
    /// private root that is only a trust anchor if the caller says so via
    /// `sslrootcert`.
    ///
    /// Every DSN below dials `127.0.0.1` literally rather than `localhost`,
    /// because `localhost` resolves to two addresses and the driver tries them
    /// in turn — the fake server accepts once, so the second attempt's
    /// "connection refused" would mask the TLS outcome under test.
    fn pki(sans: &[&str]) -> Pki {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::from_params(&ca_params, &ca_key);

        let leaf_key = KeyPair::generate().unwrap();
        let leaf_params =
            CertificateParams::new(sans.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

        Pki {
            ca_pem: ca_cert.pem(),
            leaf_der: leaf_cert.der().clone(),
            leaf_key: rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der())
                .unwrap()
                .clone_key(),
        }
    }

    /// Spawn the fake server; returns its port and a channel that reports
    /// whether the TLS handshake completed.
    fn serve(pki: &Pki) -> (u16, mpsc::Receiver<bool>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![pki.leaf_der.clone()], pki.leaf_key.clone_key())
        .unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                let _ = tx.send(false);
                return;
            };
            let mut hello = [0u8; 8];
            if sock.read_exact(&mut hello).is_err() || sock.write_all(b"S").is_err() {
                let _ = tx.send(false);
                return;
            }
            let mut conn = rustls::ServerConnection::new(std::sync::Arc::new(cfg)).unwrap();
            // Ok(_) here means the client validated us and sent its Finished;
            // Err means it walked away mid-handshake, which is a refusal.
            let _ = tx.send(conn.complete_io(&mut sock).is_ok());
        });
        (port, rx)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    /// `require` is encryption without validation — libpq's semantics, and the
    /// rung that keeps a stock RDS DSN working. The handshake must complete
    /// against a certificate signed by a CA nobody trusts.
    #[test]
    fn require_encrypts_without_validating() {
        let pki = pki(&["127.0.0.1"]);
        let (port, handshake) = serve(&pki);
        let err = connect(&rt(), &format!("postgres://u@127.0.0.1:{port}/db?sslmode=require"))
            .expect_err("the fake server hangs up after the handshake");
        assert!(handshake.recv().unwrap(), "TLS must have been established: {err}");
        assert!(
            !err.to_string().to_lowercase().contains("certificate"),
            "require must not have failed on the certificate: {err}"
        );
    }

    /// The rung that actually protects the wire. Same certificate, same
    /// server, and a name that matches — refused anyway, because its issuer is
    /// in no trust store. That isolates the issuer as the sole reason.
    #[test]
    fn verify_full_refuses_an_untrusted_issuer() {
        let pki = pki(&["127.0.0.1"]);
        let (port, handshake) = serve(&pki);
        let err = connect(&rt(), &format!("postgres://u@127.0.0.1:{port}/db?sslmode=verify-full"))
            .expect_err("an untrusted issuer must not connect");
        assert!(!handshake.recv().unwrap(), "the client must abort the handshake: {err}");
        assert!(
            err.to_string().to_lowercase().contains("certificate"),
            "the refusal must name the certificate: {err}"
        );
    }

    /// The Azure/RDS path from the issue: same untrusted CA, but named as the
    /// root. This is what makes `verify-full` usable on a managed backend.
    #[test]
    fn sslrootcert_makes_the_private_ca_trusted() {
        let pki = pki(&["127.0.0.1"]);
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, pki.ca_pem.as_bytes()).unwrap();
        let (port, handshake) = serve(&pki);
        let err = connect(
            &rt(),
            &format!(
                "postgres://u@127.0.0.1:{port}/db?sslmode=verify-full&sslrootcert={}",
                ca.display()
            ),
        )
        .expect_err("the fake server hangs up after the handshake");
        assert!(handshake.recv().unwrap(), "the named CA must be trusted: {err}");
        assert!(
            !err.to_string().to_lowercase().contains("certificate"),
            "the certificate must have verified: {err}"
        );
    }

    /// `verify-ca` keeps the chain check and drops only the name check — so the
    /// same CA on a certificate that does NOT name this host still connects,
    /// where `verify-full` would not.
    #[test]
    fn verify_ca_keeps_the_chain_and_drops_the_hostname() {
        // The leaf names `localhost`; every DSN below dials 127.0.0.1, so the
        // name cannot match while the chain still does.
        let pki = pki(&["localhost"]);
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, pki.ca_pem.as_bytes()).unwrap();
        let (port, handshake) = serve(&pki);
        let err = connect(
            &rt(),
            &format!(
                "postgres://u@127.0.0.1:{port}/db?sslmode=verify-ca&sslrootcert={}",
                ca.display()
            ),
        )
        .expect_err("the fake server hangs up after the handshake");
        assert!(handshake.recv().unwrap(), "verify-ca must ignore the name mismatch: {err}");

        let (port, handshake) = serve(&pki);
        let err = connect(
            &rt(),
            &format!(
                "postgres://u@127.0.0.1:{port}/db?sslmode=verify-full&sslrootcert={}",
                ca.display()
            ),
        )
        .expect_err("verify-full must reject the name mismatch");
        assert!(!handshake.recv().unwrap(), "verify-full must abort on the name: {err}");
    }
}
