//! Web Bot Auth: signed requests so that a site can verify that a request really came from
//! this crawler, rather than trusting the User-Agent string.
//!
//! Implements the HTTP Message Signatures profile for automated traffic
//! (draft-ietf-webbotauth-httpsig-protocol-01, October 2026) over RFC 9421 with Ed25519:
//! every request carries `Signature-Agent` (where the public key lives), `Signature-Input`
//! (what is signed and with which parameters) and `Signature`. The covered components are
//! `@authority` and the `signature-agent` field; the parameters are `created`, `expires`,
//! `keyid` (the RFC 7638 JWK thumbprint), `alg`, `nonce` and `tag="web-bot-auth"`.
//!
//! The matching public key must be published at
//! `<agent>/.well-known/http-message-signatures-directory` as a JWK Set; `rayap key` prints
//! that document. Verification is the site's side; rayap only signs.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{Error, Result};

/// How long a signature stays valid. The draft recommends at most 24 hours; a crawler's
/// request is consumed within seconds, so a few minutes leaves room for clock skew and
/// nothing more.
const VALIDITY: Duration = Duration::from_secs(5 * 60);
const LABEL: &str = "sig1";

pub struct Signer {
    key: SigningKey,
    /// Where the directory is served from, as an `https` URL (its origin is what is signed).
    agent: Url,
    keyid: String,
}

impl Signer {
    /// `secret` is the 32-byte Ed25519 seed; `agent` the https URL whose
    /// `/.well-known/http-message-signatures-directory` publishes the public key.
    pub fn new(secret: [u8; 32], agent: Url) -> Result<Self> {
        if agent.scheme() != "https" {
            return Err(Error::Config(
                "signing.agent must be an https URL (the key directory is fetched over TLS)".into(),
            ));
        }
        let key = SigningKey::from_bytes(&secret);
        let keyid = thumbprint(&key.verifying_key());
        Ok(Self { key, agent, keyid })
    }

    /// Reads the seed from a file: 32 raw bytes, or base64 (standard or URL-safe) of them.
    pub fn from_key_file(path: &std::path::Path, agent: Url) -> Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::Config(format!("signing key {}: {e}", path.display())))?;
        let seed: [u8; 32] = if bytes.len() == 32 {
            bytes.as_slice().try_into().expect("32 bytes")
        } else {
            let text = String::from_utf8_lossy(&bytes).trim().to_string();
            let decoded = URL_SAFE_NO_PAD
                .decode(&text)
                .or_else(|_| STANDARD.decode(&text))
                .map_err(|_| {
                    Error::Config(format!(
                        "signing key {}: expected 32 raw bytes or their base64",
                        path.display()
                    ))
                })?;
            decoded.as_slice().try_into().map_err(|_| {
                Error::Config(format!("signing key {}: not 32 bytes", path.display()))
            })?
        };
        Self::new(seed, agent)
    }

    /// Generates a fresh seed. Caller stores it; nothing here persists.
    pub fn generate_seed() -> [u8; 32] {
        use rand::RngCore;
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        seed
    }

    pub fn keyid(&self) -> &str {
        &self.keyid
    }

    pub fn agent(&self) -> &Url {
        &self.agent
    }

    /// The JWK Set to serve at `/.well-known/http-message-signatures-directory`.
    pub fn directory(&self) -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "OKP",
                "crv": "Ed25519",
                "kid": self.keyid,
                "x": URL_SAFE_NO_PAD.encode(self.key.verifying_key().as_bytes()),
                "use": "sig",
            }]
        })
    }

    /// The three headers for a request to `url`, signed at `now`.
    pub fn headers(&self, url: &Url, now: SystemTime) -> HeaderMap {
        self.headers_with(url, now, &random_nonce())
    }

    fn headers_with(&self, url: &Url, now: SystemTime, nonce: &str) -> HeaderMap {
        let created = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let expires = created + VALIDITY.as_secs();
        let authority = authority(url);
        let agent = self.agent.origin().ascii_serialization();
        let agent_field = format!("{LABEL}=\"{agent}\"");
        let params = format!(
            "(\"@authority\" \"signature-agent\";key=\"{LABEL}\");created={created};expires={expires};keyid=\"{}\";alg=\"ed25519\";nonce=\"{nonce}\";tag=\"web-bot-auth\"",
            self.keyid
        );
        let base = signature_base(&authority, &agent, &params);
        let signature = self.key.sign(base.as_bytes());
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("signature-agent"),
            HeaderValue::from_str(&agent_field).expect("ascii"),
        );
        headers.insert(
            HeaderName::from_static("signature-input"),
            HeaderValue::from_str(&format!("{LABEL}={params}")).expect("ascii"),
        );
        headers.insert(
            HeaderName::from_static("signature"),
            HeaderValue::from_str(&format!(
                "{LABEL}=:{}:",
                STANDARD.encode(signature.to_bytes())
            ))
            .expect("ascii"),
        );
        headers
    }
}

/// RFC 9421 §2.5 signature base for the components this profile covers. The Signature-Agent
/// value is a Structured Field dictionary member, serialised as a quoted string.
fn signature_base(authority: &str, agent: &str, params: &str) -> String {
    format!(
        "\"@authority\": {authority}\n\"signature-agent\";key=\"{LABEL}\": \"{agent}\"\n\"@signature-params\": {params}"
    )
}

/// RFC 9421 §2.2.3: host, lower-cased, with the port only if not the scheme's default.
fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    }
}

/// RFC 8037 appendix A.3 / RFC 7638: SHA-256 of the canonical JWK
/// `{"crv":"Ed25519","kty":"OKP","x":"<base64url>"}`, base64url without padding.
fn thumbprint(key: &VerifyingKey) -> String {
    let canonical = format!(
        "{{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":\"{}\"}}",
        URL_SAFE_NO_PAD.encode(key.as_bytes())
    );
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

fn random_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 9421 appendix B.1.4 test key, as used by the Web Bot Auth draft's vectors.
    const TEST_SEED_PKCS8_B64: &str =
        "MC4CAQAwBQYDK2VwBCIEIJ+DYvh6SEqVTm50DFtMDoQikTmiCqirVv9mWG9qfSnF";
    const TEST_X: &str = "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs";
    const TEST_KEYID: &str = "poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U";

    fn test_seed() -> [u8; 32] {
        // PKCS#8 wrapper: the last 32 bytes are the seed.
        let der = STANDARD.decode(TEST_SEED_PKCS8_B64).unwrap();
        der[der.len() - 32..].try_into().unwrap()
    }

    fn signer() -> Signer {
        Signer::new(
            test_seed(),
            Url::parse("https://signature-agent.test").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn thumbprint_and_public_key_match_the_drafts_vector() {
        let s = signer();
        assert_eq!(s.keyid(), TEST_KEYID);
        assert_eq!(s.directory()["keys"][0]["x"], TEST_X);
        assert_eq!(s.directory()["keys"][0]["kid"], TEST_KEYID);
    }

    #[test]
    fn signs_the_drafts_ed25519_test_vector_byte_for_byte() {
        // draft-ietf-webbotauth-httpsig-protocol-01 appendix E.2.1, with label sig2. Ed25519
        // is deterministic, so the same base must give the same signature.
        let s = Signer::new(
            test_seed(),
            Url::parse("https://signature-agent.test").unwrap(),
        )
        .unwrap();
        let nonce = "n9p433xm+NJ3ph3upfBIGmsuwHw387YV7Q/F+6BSpGCVjYCqQw6rznNA8PVVLySrAWsv0hQtFioQb6E1YsauiA==";
        let params = format!(
            "(\"@authority\" \"signature-agent\";key=\"sig2\");created=1735689600;keyid=\"{TEST_KEYID}\";alg=\"ed25519\";expires=4889289600;nonce=\"{nonce}\";tag=\"web-bot-auth\""
        );
        let base = format!(
            "\"@authority\": example.com\n\"signature-agent\";key=\"sig2\": \"https://signature-agent.test\"\n\"@signature-params\": {params}"
        );
        let sig = s.key.sign(base.as_bytes());
        assert_eq!(
            STANDARD.encode(sig.to_bytes()),
            "CGokAoY/FrPpyuzTe1Fb2kLTAH9Idw6qSFd1+D77r1firsMRCxeuLu5e6O+ek3zQgDMAzjiXtLVfqE00xnABCQ=="
        );
    }

    #[test]
    fn request_headers_verify_with_the_published_key() {
        let s = signer();
        let url = Url::parse("https://Example.COM:8443/path?q=1").unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let headers = s.headers_with(&url, now, "nonce-1");
        assert_eq!(
            headers["signature-agent"],
            "sig1=\"https://signature-agent.test\""
        );
        let input = headers["signature-input"].to_str().unwrap();
        assert!(input.starts_with("sig1=(\"@authority\" \"signature-agent\";key=\"sig1\");created=1800000000;expires=1800000300;keyid=\""));
        assert!(input.contains("tag=\"web-bot-auth\""));
        // Rebuild the base as a verifier would and check with the directory's public key.
        let params = input.strip_prefix("sig1=").unwrap();
        let base = signature_base("example.com:8443", "https://signature-agent.test", params);
        let sig_b64 = headers["signature"].to_str().unwrap();
        let sig_bytes = STANDARD
            .decode(sig_b64.trim_start_matches("sig1=:").trim_end_matches(':'))
            .unwrap();
        let x = URL_SAFE_NO_PAD
            .decode(s.directory()["keys"][0]["x"].as_str().unwrap())
            .unwrap();
        let vk = VerifyingKey::from_bytes(x.as_slice().try_into().unwrap()).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        vk.verify_strict(base.as_bytes(), &sig).expect("verifies");
        // A different authority does not verify against the same signature.
        let other = signature_base("evil.example", "https://signature-agent.test", params);
        assert!(vk.verify_strict(other.as_bytes(), &sig).is_err());
    }

    #[test]
    fn agent_must_be_https_and_default_ports_are_dropped() {
        assert!(Signer::new([7u8; 32], Url::parse("http://agent.test").unwrap()).is_err());
        assert_eq!(
            authority(&Url::parse("https://A.org:443/x").unwrap()),
            "a.org"
        );
        assert_eq!(
            authority(&Url::parse("http://a.org:8080/x").unwrap()),
            "a.org:8080"
        );
    }
}
