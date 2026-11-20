// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Loading and validating the device's x.509 certificate and private key.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::AppError;

/// A parsed client certificate.
#[derive(Debug, Clone)]
pub struct LocalCertificate {
    /// Original PEM bytes (passed to the TLS stack).
    pub pem: Vec<u8>,
    /// Lowercase hex SHA-256 of the leaf certificate's DER. AWS IoT uses this as the certificate ID.
    pub certificate_id: String,
    pub subject: String,
    pub issuer: String,
    pub not_before: String,
    pub not_after: String,
    /// True if the current time is within the validity window.
    pub currently_valid: bool,
}

/// Private key encoding found in the PEM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Rsa,
    Ec,
    Pkcs8,
}

impl KeyKind {
    pub fn label(self) -> &'static str {
        match self {
            KeyKind::Rsa => "RSA (PKCS#1)",
            KeyKind::Ec => "EC (SEC1)",
            KeyKind::Pkcs8 => "PKCS#8",
        }
    }
}

/// A validated private key. The PEM is kept in memory only; it is never persisted by the app.
#[derive(Clone)]
pub struct LocalPrivateKey {
    pub pem: Vec<u8>,
    pub kind: KeyKind,
}

impl std::fmt::Debug for LocalPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalPrivateKey")
            .field("kind", &self.kind)
            .field("pem", &"<redacted>")
            .finish()
    }
}

pub fn read_file(path: &Path) -> Result<Vec<u8>, AppError> {
    std::fs::read(path).map_err(|e| {
        AppError::configuration(format!("Could not read {}", path.display()))
            .with_details(e.to_string())
            .with_action("Check the file path and permissions.")
    })
}

/// Parses a PEM-encoded certificate (chain). The first certificate is treated as the leaf.
pub fn parse_certificate(pem: &[u8]) -> Result<LocalCertificate, AppError> {
    let invalid = |details: String| {
        AppError::authentication("Invalid x.509 certificate")
            .with_details(details)
            .with_action("Provide a PEM-encoded certificate (-----BEGIN CERTIFICATE-----).")
    };

    let ders = rustls_pemfile::certs(&mut &pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("PEM parse error: {e}")))?;
    let leaf = ders
        .first()
        .ok_or_else(|| invalid("No CERTIFICATE block found".into()))?;

    let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|e| invalid(format!("DER parse error: {e}")))?;

    let validity = cert.validity();
    Ok(LocalCertificate {
        pem: pem.to_vec(),
        certificate_id: sha256_hex(leaf.as_ref()),
        subject: cert.subject().to_string(),
        issuer: cert.issuer().to_string(),
        not_before: validity.not_before.to_string(),
        not_after: validity.not_after.to_string(),
        currently_valid: validity.is_valid(),
    })
}

/// Parses a PEM-encoded private key (PKCS#1 RSA, SEC1 EC, or PKCS#8).
pub fn parse_private_key(pem: &[u8]) -> Result<LocalPrivateKey, AppError> {
    let invalid = |details: String| {
        AppError::authentication("Invalid private key")
            .with_details(details)
            .with_action("Provide an unencrypted PEM private key (RSA, EC, or PKCS#8).")
    };

    let mut reader = pem;
    loop {
        match rustls_pemfile::read_one(&mut reader) {
            Ok(Some(rustls_pemfile::Item::Pkcs1Key(_))) => {
                return Ok(LocalPrivateKey {
                    pem: pem.to_vec(),
                    kind: KeyKind::Rsa,
                })
            }
            Ok(Some(rustls_pemfile::Item::Sec1Key(_))) => {
                return Ok(LocalPrivateKey {
                    pem: pem.to_vec(),
                    kind: KeyKind::Ec,
                })
            }
            Ok(Some(rustls_pemfile::Item::Pkcs8Key(_))) => {
                return Ok(LocalPrivateKey {
                    pem: pem.to_vec(),
                    kind: KeyKind::Pkcs8,
                })
            }
            Ok(Some(_)) => continue,
            Ok(None) => {
                let hint = if String::from_utf8_lossy(pem).contains("ENCRYPTED PRIVATE KEY") {
                    "Encrypted private keys are not supported; decrypt it first (openssl pkcs8 -in key.pem -out key-plain.pem)."
                } else {
                    "No PRIVATE KEY block found"
                };
                return Err(invalid(hint.into()));
            }
            Err(e) => return Err(invalid(format!("PEM parse error: {e}"))),
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Amazon Root CA 1 — a real, public certificate used here only as parse-test input.
    const SAMPLE_CERT: &[u8] = include_bytes!("../assets/AmazonRootCA1.pem");

    #[test]
    fn parses_certificate_and_computes_id() {
        let c = parse_certificate(SAMPLE_CERT).expect("parse");
        assert_eq!(c.certificate_id.len(), 64);
        assert!(c.certificate_id.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(c.subject.contains("Amazon Root CA 1"));
        // Well-known SHA-256 fingerprint of Amazon Root CA 1.
        assert_eq!(
            c.certificate_id,
            "8ecde6884f3d87b1125ba31ac3fcb13d7016de7f57cc904fe1cb97c6ae98196e"
        );
    }

    #[test]
    fn rejects_garbage_certificate() {
        assert!(parse_certificate(b"not a cert").is_err());
        assert!(parse_certificate(
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"
        )
        .is_err());
    }

    #[test]
    fn key_kinds_detected() {
        // Bodies only need to be valid base64 for PEM framing; content isn't checked here.
        let wrap = |label: &str| format!("-----BEGIN {label}-----\nAAAA\n-----END {label}-----\n");
        assert_eq!(
            parse_private_key(wrap("RSA PRIVATE KEY").as_bytes())
                .unwrap()
                .kind,
            KeyKind::Rsa
        );
        assert_eq!(
            parse_private_key(wrap("EC PRIVATE KEY").as_bytes())
                .unwrap()
                .kind,
            KeyKind::Ec
        );
        assert_eq!(
            parse_private_key(wrap("PRIVATE KEY").as_bytes())
                .unwrap()
                .kind,
            KeyKind::Pkcs8
        );
    }

    #[test]
    fn rejects_cert_as_key_and_encrypted_key() {
        assert!(parse_private_key(SAMPLE_CERT).is_err());
        let enc =
            b"-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n";
        let err = parse_private_key(enc).unwrap_err();
        assert!(err.details.unwrap().contains("Encrypted"));
    }

    #[test]
    fn key_debug_is_redacted() {
        let k = LocalPrivateKey {
            pem: b"secret".to_vec(),
            kind: KeyKind::Rsa,
        };
        assert!(!format!("{k:?}").contains("secret"));
    }
}
