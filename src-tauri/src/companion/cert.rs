//! The self-signed TLS certificate. Browsers gate getUserMedia on secure
//! contexts, so a plain http:// LAN origin cannot open the mic at all;
//! the companion server therefore speaks TLS with a self-signed cert
//! generated once, persisted in the app data dir, and verified by the
//! user against the SHA-256 fingerprint shown in Settings on the phone's
//! first-visit interstitial.

use std::path::Path;

use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// A loaded (generated or read-back) certificate ready for the TLS acceptor.
pub struct CompanionCert {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    /// Lowercase hex SHA-256 of the DER cert, shown in Settings.
    pub fingerprint: String,
}

const CERT_FILE: &str = "companion-cert.pem";
const KEY_FILE: &str = "companion-key.pem";

/// Load the persisted certificate from `dir`, generating and persisting a
/// fresh one on first run (or when the files are unreadable). SANs cover
/// the LAN IP (re-derived at generation time) plus a stable DNS name.
pub fn ensure_certificate(dir: &Path, lan_ip: std::net::IpAddr) -> Result<CompanionCert, String> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);

    if let Some(loaded) = load_persisted(&cert_path, &key_path) {
        return Ok(loaded);
    }

    generate_and_persist(&cert_path, &key_path, lan_ip)
}

fn load_persisted(cert_path: &Path, key_path: &Path) -> Option<CompanionCert> {
    let cert_pem = std::fs::read_to_string(cert_path).ok()?;
    let key_pem = std::fs::read_to_string(key_path).ok()?;
    let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes()).ok()?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).ok()?;
    let fingerprint = fingerprint_hex(cert.as_ref());
    log::info!("companion: loaded persisted TLS certificate ({fingerprint})");
    Some(CompanionCert {
        cert,
        key,
        fingerprint,
    })
}

fn generate_and_persist(
    cert_path: &Path,
    key_path: &Path,
    lan_ip: std::net::IpAddr,
) -> Result<CompanionCert, String> {
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "VoxBar Companion");
    // SANs: the LAN IP the QR advertises (so the cert itself matches the
    // visited origin) and a stable name for the rare browser that prefers
    // one. NotCriticalBefore/After defaults are fine for a LAN appliance.
    params.subject_alt_names = vec![
        rcgen::SanType::IpAddress(lan_ip),
        rcgen::SanType::DnsName("voxbar.local".try_into().map_err(|e| format!("{e}"))?),
    ];

    let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("key generation failed: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("certificate generation failed: {e}"))?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
    }
    std::fs::write(cert_path, &cert_pem).map_err(|e| format!("cert write failed: {e}"))?;
    std::fs::write(key_path, &key_pem).map_err(|e| format!("key write failed: {e}"))?;
    restrict_permissions(cert_path);
    restrict_permissions(key_path);

    let der = cert.der().clone();
    let fingerprint = fingerprint_hex(der.as_ref());
    log::info!("companion: generated new TLS certificate ({fingerprint})");
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).map_err(|e| format!("{e}"))?;
    Ok(CompanionCert {
        cert: der,
        key,
        fingerprint,
    })
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// Lowercase hex SHA-256 of the DER certificate. Grouped in the settings
/// panel for readability; this is the raw form.
pub fn fingerprint_hex(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn certificate_round_trips_through_pem_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ip = std::net::IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));

        let first = ensure_certificate(dir.path(), ip).expect("first generation");
        assert_eq!(first.fingerprint.len(), 64);
        assert!(first.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));

        // The second load returns the SAME certificate (persisted, so the
        // phone's one-time interstitial acceptance sticks).
        let second = ensure_certificate(dir.path(), ip).expect("persisted load");
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.cert.as_ref(), second.cert.as_ref());
    }

    #[test]
    fn a_corrupt_certificate_regenerates_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ip = std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let first = ensure_certificate(dir.path(), ip).expect("first");
        std::fs::write(dir.path().join(CERT_FILE), "not a certificate").unwrap();
        let second = ensure_certificate(dir.path(), ip).expect("regenerated");
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn fingerprint_is_sha256_hex_of_der() {
        let der = [0u8; 3];
        assert_eq!(fingerprint_hex(&der).len(), 64);
        // Deterministic.
        assert_eq!(fingerprint_hex(&der), fingerprint_hex(&der));
    }
}
