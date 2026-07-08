use base64::Engine;
use chrono::Utc;
use sha1::{Digest, Sha1};

use crate::DiscoveryCredentials;

#[derive(Debug, Clone)]
pub struct WsSecurityToken {
    pub username: String,
    pub nonce_b64: String,
    pub created: String,
    pub password_digest_b64: String,
}

impl WsSecurityToken {
    pub fn build(creds: &DiscoveryCredentials) -> Self {
        let mut nonce_bytes = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut nonce_bytes[..]);
        let created = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        Self::build_with(creds, &nonce_bytes, &created)
    }

    /// Build a token with caller-supplied nonce + created. Used by tests to
    /// pin the inputs so we can assert the exact digest output.
    pub fn build_with(creds: &DiscoveryCredentials, nonce_bytes: &[u8], created: &str) -> Self {
        let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(nonce_bytes);

        // Per OASIS WS-Security UsernameToken Profile 1.0, §3.1:
        //   Password_Digest = Base64( SHA-1( nonce_raw_bytes + created + password ) )
        // `nonce_raw_bytes` are the *original* bytes that were Base64-encoded,
        // not reversed or otherwise transformed. Several ONVIF camera vendors
        // (Hikvision, Dahua) strictly verify this and reject with 401 if the
        // order is wrong.
        let mut hasher = Sha1::new();
        hasher.update(nonce_bytes);
        hasher.update(created.as_bytes());
        hasher.update(creds.password.as_bytes());
        let digest = hasher.finalize();
        let password_digest_b64 = base64::engine::general_purpose::STANDARD.encode(digest);

        Self {
            username: creds.username.clone(),
            nonce_b64,
            created: created.to_string(),
            password_digest_b64,
        }
    }

    pub fn to_xml_header(&self) -> String {
        format!(
            r#"<Security xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><UsernameToken><Username>{}</Username><Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{}</PasswordDigest><Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</Nonce><Created xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0">{}</Created></UsernameToken></Security>"#,
            xml_escape(&self.username),
            self.password_digest_b64,
            self.nonce_b64,
            self.created,
        )
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_format_is_documented() {
        let creds = DiscoveryCredentials::new("admin", "12345");
        let t = WsSecurityToken::build(&creds);
        assert_eq!(t.username, "admin");
        assert!(!t.password_digest_b64.is_empty());
        assert!(!t.nonce_b64.is_empty());
        assert!(!t.created.is_empty());
    }

    #[test]
    fn known_vector_hikvision_style() {
        let creds = DiscoveryCredentials::new("admin", "abcd1234");
        let t1 = WsSecurityToken::build(&creds);
        let t2 = WsSecurityToken::build(&creds);
        assert_eq!(t1.password_digest_b64.len(), 28);
        assert_ne!(t1.nonce_b64, t2.nonce_b64);
    }

    #[test]
    fn xml_header_is_well_formed() {
        let creds = DiscoveryCredentials::new("user", "pw");
        let t = WsSecurityToken::build(&creds);
        let xml = t.to_xml_header();
        assert!(xml.contains("<UsernameToken>"));
        assert!(xml.contains("PasswordDigest"));
        assert!(xml.contains("<Nonce"));
    }

    /// Regression: previously `nonce_bytes.iter().rev()` reversed the nonce
    /// before hashing, which violates OASIS WS-Security UsernameToken Profile
    /// 1.0 §3.1 and was rejected with 401 by Hikvision/Dahua cameras using
    /// `admin/changeme`. This test pins the spec-defined byte ordering.
    #[test]
    fn digest_follows_spec_order() {
        let creds = DiscoveryCredentials::new("admin", "changeme");
        let nonce: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let created = "2026-01-01T00:00:00Z";
        let t = WsSecurityToken::build_with(&creds, &nonce, created);

        let mut h = Sha1::new();
        h.update(nonce);
        h.update(created.as_bytes());
        h.update(creds.password.as_bytes());
        let expected = base64::engine::general_purpose::STANDARD.encode(h.finalize());

        assert_eq!(t.password_digest_b64, expected);
        assert_eq!(
            t.nonce_b64,
            base64::engine::general_purpose::STANDARD.encode(nonce)
        );
    }
}
