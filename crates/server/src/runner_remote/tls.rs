//! Private CA and server certificate for the remote Runner endpoint.
//! Nodes pin the CA by SHA-256, so public roots never vouch for this listener.
use chrono::{Datelike, TimeZone, Utc};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    IsCa, Issuer, KeyPair, KeyUsagePurpose, date_time_ymd,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio_rustls::rustls::pki_types::{CertificateDer, pem::PemObject};

const CA_YEARS: i32 = 10;
const SERVER_DAYS: i64 = 397;
const RENEW_BEFORE: i64 = 30 * 24 * 3600;
const LIMIT: usize = 64 * 1024;

pub struct Material {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint: String,
    pub names: Vec<String>,
    pub expires_at: i64,
}
#[derive(Serialize, Deserialize)]
struct ServerMeta {
    names: Vec<String>,
    expires_at: i64,
    ca_fingerprint: String,
}

pub fn directory(root: &Path) -> PathBuf {
    root.join("remote-runner-tls")
}
fn midnight(timestamp: i64) -> (i32, u8, u8, i64) {
    let day = Utc.timestamp_opt(timestamp, 0).single().unwrap_or_else(Utc::now);
    let start = Utc
        .with_ymd_and_hms(day.year(), day.month(), day.day(), 0, 0, 0)
        .single()
        .map(|d| d.timestamp())
        .unwrap_or(timestamp);
    (day.year(), day.month() as u8, day.day() as u8, start)
}
fn ca_params(installation: &str) -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::OrganizationName, "Muteki");
    name.push(
        DnType::CommonName,
        format!("Muteki Runner CA {}", installation.chars().take(12).collect::<String>()),
    );
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}
pub fn fingerprint(pem: &str) -> Result<String, String> {
    let der = CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", Sha256::digest(der.as_ref())))
}
fn read(root: &Path, path: &Path) -> Result<Option<String>, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
        Ok(_) => String::from_utf8(crate::private_files::read(root, path, LIMIT)?)
            .map(Some)
            .map_err(|e| e.to_string()),
    }
}
fn write(root: &Path, path: &Path, value: &str) -> Result<(), String> {
    crate::private_files::write(root, path, value.as_bytes())
}

/// Loads the CA (creating it once) and reissues the server certificate when its names change
/// or it nears expiry. The CA key never leaves the state directory.
pub fn ensure(root: &Path, installation: &str, names: &[String]) -> Result<Material, String> {
    let dir = directory(root);
    crate::private_files::ensure_dir(root, &dir)?;
    let now = Utc::now().timestamp();
    let (ca_pem, ca_key) = match (read(root, &dir.join("ca.pem"))?, read(root, &dir.join("ca.key"))?) {
        (Some(pem), Some(key)) => (pem, KeyPair::from_pem(&key).map_err(|e| e.to_string())?),
        (None, None) => {
            let key = KeyPair::generate().map_err(|e| e.to_string())?;
            let mut params = ca_params(installation);
            let (y, m, d, _) = midnight(now - 24 * 3600);
            params.not_before = date_time_ymd(y, m, d);
            params.not_after = date_time_ymd(y + CA_YEARS, m, d.min(28));
            let pem = params.self_signed(&key).map_err(|e| e.to_string())?.pem();
            write(root, &dir.join("ca.key"), &key.serialize_pem())?;
            write(root, &dir.join("ca.pem"), &pem)?;
            (pem, key)
        }
        _ => return Err("Runner CA material is incomplete; restore both ca.pem and ca.key or remove both to issue a new CA".into()),
    };
    let ca_fingerprint = fingerprint(&ca_pem)?;
    let mut names = names.to_vec();
    names.sort();
    names.dedup();
    if let (Some(cert_pem), Some(key_pem), Some(meta)) = (
        read(root, &dir.join("server.pem"))?,
        read(root, &dir.join("server.key"))?,
        read(root, &dir.join("server.json"))?,
    ) {
        if let Ok(meta) = serde_json::from_str::<ServerMeta>(&meta) {
            if meta.names == names && meta.ca_fingerprint == ca_fingerprint && meta.expires_at - RENEW_BEFORE > now {
                return Ok(Material { ca_pem, cert_pem, key_pem, fingerprint: ca_fingerprint, names, expires_at: meta.expires_at });
            }
        }
    }
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = CertificateParams::new(names.clone()).map_err(|e| format!("Invalid certificate name: {e}"))?;
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, names.first().cloned().unwrap_or_else(|| "muteki-runner".into()));
    params.distinguished_name = subject;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    let (y, m, d, _) = midnight(now - 24 * 3600);
    params.not_before = date_time_ymd(y, m, d);
    let (ey, em, ed, expires_at) = midnight(now + SERVER_DAYS * 24 * 3600);
    params.not_after = date_time_ymd(ey, em, ed);
    let issuer = Issuer::new(ca_params(installation), ca_key);
    let cert_pem = params.signed_by(&key, &issuer).map_err(|e| e.to_string())?.pem();
    let key_pem = key.serialize_pem();
    write(root, &dir.join("server.key"), &key_pem)?;
    write(root, &dir.join("server.pem"), &cert_pem)?;
    let meta = ServerMeta { names: names.clone(), expires_at, ca_fingerprint: ca_fingerprint.clone() };
    write(root, &dir.join("server.json"), &serde_json::to_string(&meta).map_err(|e| e.to_string())?)?;
    Ok(Material { ca_pem, cert_pem, key_pem, fingerprint: ca_fingerprint, names, expires_at })
}
/// CA certificate for out-of-band `--ca-file` installs; never the key.
pub fn ca_pem(root: &Path) -> Result<Option<String>, String> {
    read(root, &directory(root).join("ca.pem"))
}
