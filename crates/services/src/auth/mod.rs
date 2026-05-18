use argon2::password_hash::rand_core::OsRng;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use bson::oid::ObjectId;
use chrono::{Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use roomler_ai_config::JwtSettings;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Invalid credentials")]
    InvalidCredentials,
    #[error("Token expired")]
    TokenExpired,
    #[error("Invalid token: {0}")]
    InvalidToken(String),
    #[error("Password hash error: {0}")]
    HashError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user_id
    pub email: String,
    pub username: String,
    pub iat: i64,
    pub exp: i64,
    pub iss: String,
    pub token_type: TokenType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TokenType {
    Access,
    Refresh,
    /// Single-use, short-lived token used to enroll a remote-control agent.
    Enrollment,
    /// Long-lived token carried by an enrolled remote-control agent.
    Agent,
    /// Single-use, short-lived bootstrap token an admin issues to the
    /// operator. Exchanged for a long-lived `TunnelClient` token via
    /// `POST /api/tunnel-client/enroll`.
    TunnelEnrollment,
    /// Long-lived token carried by an enrolled `roomler-tunnel` client
    /// on its WebSocket connection (`role=tunnel-client`). Audience
    /// distinct from `Agent` — agents serve forwards, clients open them.
    TunnelClient,
}

/// Claims carried by a remote-control enrollment token (aud = enroll).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentClaims {
    pub sub: String, // issuer-user id (admin who created the token)
    pub tenant_id: String,
    pub iat: i64,
    pub exp: i64,
    pub iss: String,
    pub token_type: TokenType,
    pub jti: String, // unique id — caller may persist for single-use checks
}

/// Claims carried by a remote-control agent token (aud = agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentClaims {
    pub sub: String, // agent_id hex
    pub tenant_id: String,
    pub iat: i64,
    pub exp: i64,
    pub iss: String,
    pub token_type: TokenType,
}

/// Claims carried by a `roomler-tunnel` client token. Long-lived,
/// one per enrolled laptop. `owner_user_id` lets the WS handler
/// associate every forward decision with the operating user for
/// audit + policy evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelClientClaims {
    /// `tunnel_client._id` hex.
    pub sub: String,
    pub tenant_id: String,
    /// User who installed and runs this CLI. Recorded in
    /// `tunnel_audit` rows alongside every forward decision.
    pub owner_user_id: String,
    pub iat: i64,
    pub exp: i64,
    pub iss: String,
    pub token_type: TokenType,
}

/// Claims carried by a tunnel-enrollment token. Mirrors
/// [`EnrollmentClaims`] (single-use via `jti`, short TTL) but its
/// own audience so a leaked agent-enrollment can't bootstrap a
/// tunnel client and vice-versa.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelEnrollmentClaims {
    /// Admin user id (issuer) as hex.
    pub sub: String,
    pub tenant_id: String,
    pub iat: i64,
    pub exp: i64,
    pub iss: String,
    pub token_type: TokenType,
    /// Unique id — caller may persist for single-use checks.
    pub jti: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

pub struct AuthService {
    jwt_settings: JwtSettings,
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
}

impl AuthService {
    pub fn new(jwt_settings: JwtSettings) -> Self {
        let encoding_key = EncodingKey::from_secret(jwt_settings.secret.as_bytes());
        let decoding_key = DecodingKey::from_secret(jwt_settings.secret.as_bytes());
        Self {
            jwt_settings,
            encoding_key,
            decoding_key,
        }
    }

    pub fn hash_password(&self, password: &str) -> Result<String, AuthError> {
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| AuthError::HashError(e.to_string()))?;
        Ok(hash.to_string())
    }

    pub fn verify_password(&self, password: &str, hash: &str) -> Result<bool, AuthError> {
        let parsed_hash =
            PasswordHash::new(hash).map_err(|e| AuthError::HashError(e.to_string()))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .is_ok())
    }

    pub fn generate_tokens(
        &self,
        user_id: ObjectId,
        email: &str,
        username: &str,
    ) -> Result<TokenPair, AuthError> {
        let now = Utc::now();

        let access_claims = Claims {
            sub: user_id.to_hex(),
            email: email.to_string(),
            username: username.to_string(),
            iat: now.timestamp(),
            exp: (now + Duration::seconds(self.jwt_settings.access_token_ttl_secs as i64))
                .timestamp(),
            iss: self.jwt_settings.issuer.clone(),
            token_type: TokenType::Access,
        };

        let refresh_claims = Claims {
            sub: user_id.to_hex(),
            email: email.to_string(),
            username: username.to_string(),
            iat: now.timestamp(),
            exp: (now + Duration::seconds(self.jwt_settings.refresh_token_ttl_secs as i64))
                .timestamp(),
            iss: self.jwt_settings.issuer.clone(),
            token_type: TokenType::Refresh,
        };

        let access_token = encode(&Header::default(), &access_claims, &self.encoding_key)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;

        let refresh_token = encode(&Header::default(), &refresh_claims, &self.encoding_key)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;

        Ok(TokenPair {
            access_token,
            refresh_token,
            expires_in: self.jwt_settings.access_token_ttl_secs,
        })
    }

    pub fn verify_token(&self, token: &str) -> Result<Claims, AuthError> {
        let mut validation = Validation::default();
        validation.set_issuer(&[&self.jwt_settings.issuer]);

        let token_data = decode::<Claims>(token, &self.decoding_key, &validation).map_err(|e| {
            match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                _ => AuthError::InvalidToken(e.to_string()),
            }
        })?;

        Ok(token_data.claims)
    }

    pub fn verify_access_token(&self, token: &str) -> Result<Claims, AuthError> {
        let claims = self.verify_token(token)?;
        if claims.token_type != TokenType::Access {
            return Err(AuthError::InvalidToken("Not an access token".to_string()));
        }
        Ok(claims)
    }

    pub fn verify_refresh_token(&self, token: &str) -> Result<Claims, AuthError> {
        let claims = self.verify_token(token)?;
        if claims.token_type != TokenType::Refresh {
            return Err(AuthError::InvalidToken("Not a refresh token".to_string()));
        }
        Ok(claims)
    }

    // ─── Remote-control tokens ────────────────────────────────────────

    /// Mint a single-use enrollment token. The returned `jti` is unique and
    /// may be persisted by the caller for replay protection.
    pub fn issue_enrollment_token(
        &self,
        admin_user_id: ObjectId,
        tenant_id: ObjectId,
        ttl_secs: u64,
    ) -> Result<(String, String), AuthError> {
        let now = Utc::now();
        let jti = uuid_v4_hex();
        let claims = EnrollmentClaims {
            sub: admin_user_id.to_hex(),
            tenant_id: tenant_id.to_hex(),
            iat: now.timestamp(),
            exp: (now + Duration::seconds(ttl_secs as i64)).timestamp(),
            iss: self.jwt_settings.issuer.clone(),
            token_type: TokenType::Enrollment,
            jti: jti.clone(),
        };
        let token = encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
        Ok((token, jti))
    }

    pub fn verify_enrollment_token(&self, token: &str) -> Result<EnrollmentClaims, AuthError> {
        let mut validation = Validation::default();
        validation.set_issuer(&[&self.jwt_settings.issuer]);
        let data = decode::<EnrollmentClaims>(token, &self.decoding_key, &validation).map_err(
            |e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                _ => AuthError::InvalidToken(e.to_string()),
            },
        )?;
        if data.claims.token_type != TokenType::Enrollment {
            return Err(AuthError::InvalidToken(
                "Not an enrollment token".to_string(),
            ));
        }
        Ok(data.claims)
    }

    /// Mint a long-lived agent token (default TTL from settings.refresh_token_ttl_secs
    /// unless `override_ttl_secs` is provided).
    pub fn issue_agent_token(
        &self,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        override_ttl_secs: Option<u64>,
    ) -> Result<String, AuthError> {
        let now = Utc::now();
        let ttl = override_ttl_secs.unwrap_or(365 * 24 * 60 * 60); // 1 year default
        let claims = AgentClaims {
            sub: agent_id.to_hex(),
            tenant_id: tenant_id.to_hex(),
            iat: now.timestamp(),
            exp: (now + Duration::seconds(ttl as i64)).timestamp(),
            iss: self.jwt_settings.issuer.clone(),
            token_type: TokenType::Agent,
        };
        encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))
    }

    pub fn verify_agent_token(&self, token: &str) -> Result<AgentClaims, AuthError> {
        let mut validation = Validation::default();
        validation.set_issuer(&[&self.jwt_settings.issuer]);
        let data = decode::<AgentClaims>(token, &self.decoding_key, &validation).map_err(|e| {
            match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                _ => AuthError::InvalidToken(e.to_string()),
            }
        })?;
        if data.claims.token_type != TokenType::Agent {
            return Err(AuthError::InvalidToken("Not an agent token".to_string()));
        }
        Ok(data.claims)
    }

    // ─── roomler-tunnel client tokens ─────────────────────────────────

    /// Mint a single-use tunnel-enrollment token. Returned `jti` is
    /// unique; caller may persist for replay protection. Mirrors
    /// [`issue_enrollment_token`] but with a distinct audience.
    pub fn issue_tunnel_enrollment_token(
        &self,
        admin_user_id: ObjectId,
        tenant_id: ObjectId,
        ttl_secs: u64,
    ) -> Result<(String, String), AuthError> {
        let now = Utc::now();
        let jti = uuid_v4_hex();
        let claims = TunnelEnrollmentClaims {
            sub: admin_user_id.to_hex(),
            tenant_id: tenant_id.to_hex(),
            iat: now.timestamp(),
            exp: (now + Duration::seconds(ttl_secs as i64)).timestamp(),
            iss: self.jwt_settings.issuer.clone(),
            token_type: TokenType::TunnelEnrollment,
            jti: jti.clone(),
        };
        let token = encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
        Ok((token, jti))
    }

    pub fn verify_tunnel_enrollment_token(
        &self,
        token: &str,
    ) -> Result<TunnelEnrollmentClaims, AuthError> {
        let mut validation = Validation::default();
        validation.set_issuer(&[&self.jwt_settings.issuer]);
        let data = decode::<TunnelEnrollmentClaims>(token, &self.decoding_key, &validation)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                _ => AuthError::InvalidToken(e.to_string()),
            })?;
        if data.claims.token_type != TokenType::TunnelEnrollment {
            return Err(AuthError::InvalidToken(
                "Not a tunnel-enrollment token".to_string(),
            ));
        }
        Ok(data.claims)
    }

    /// Mint a long-lived tunnel-client token (default TTL 1 year, override
    /// via `override_ttl_secs`). Mirrors [`issue_agent_token`].
    pub fn issue_tunnel_client_token(
        &self,
        tunnel_client_id: ObjectId,
        tenant_id: ObjectId,
        owner_user_id: ObjectId,
        override_ttl_secs: Option<u64>,
    ) -> Result<String, AuthError> {
        let now = Utc::now();
        let ttl = override_ttl_secs.unwrap_or(365 * 24 * 60 * 60);
        let claims = TunnelClientClaims {
            sub: tunnel_client_id.to_hex(),
            tenant_id: tenant_id.to_hex(),
            owner_user_id: owner_user_id.to_hex(),
            iat: now.timestamp(),
            exp: (now + Duration::seconds(ttl as i64)).timestamp(),
            iss: self.jwt_settings.issuer.clone(),
            token_type: TokenType::TunnelClient,
        };
        encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))
    }

    pub fn verify_tunnel_client_token(&self, token: &str) -> Result<TunnelClientClaims, AuthError> {
        let mut validation = Validation::default();
        validation.set_issuer(&[&self.jwt_settings.issuer]);
        let data =
            decode::<TunnelClientClaims>(token, &self.decoding_key, &validation).map_err(|e| {
                match e.kind() {
                    jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                    _ => AuthError::InvalidToken(e.to_string()),
                }
            })?;
        if data.claims.token_type != TokenType::TunnelClient {
            return Err(AuthError::InvalidToken(
                "Not a tunnel-client token".to_string(),
            ));
        }
        Ok(data.claims)
    }
}

fn uuid_v4_hex() -> String {
    // Use `rand` via argon2's OsRng — avoids pulling in the uuid crate here just for a nonce.
    use argon2::password_hash::rand_core::RngCore;
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> AuthService {
        AuthService::new(JwtSettings {
            secret: "test-secret-for-unit-tests-do-not-use-in-prod".to_string(),
            access_token_ttl_secs: 3600,
            refresh_token_ttl_secs: 604_800,
            issuer: "roomler-ai-test".to_string(),
        })
    }

    #[test]
    fn agent_token_roundtrip() {
        let s = svc();
        let agent_id = ObjectId::new();
        let tenant_id = ObjectId::new();
        let token = s.issue_agent_token(agent_id, tenant_id, Some(60)).unwrap();
        let claims = s.verify_agent_token(&token).unwrap();
        assert_eq!(claims.sub, agent_id.to_hex());
        assert_eq!(claims.tenant_id, tenant_id.to_hex());
        assert_eq!(claims.token_type, TokenType::Agent);
    }

    #[test]
    fn enrollment_token_roundtrip() {
        let s = svc();
        let admin = ObjectId::new();
        let tenant = ObjectId::new();
        let (token, jti) = s.issue_enrollment_token(admin, tenant, 600).unwrap();
        let claims = s.verify_enrollment_token(&token).unwrap();
        assert_eq!(claims.sub, admin.to_hex());
        assert_eq!(claims.tenant_id, tenant.to_hex());
        assert_eq!(claims.jti, jti);
        assert_eq!(claims.token_type, TokenType::Enrollment);
    }

    #[test]
    fn agent_token_rejects_user_token() {
        let s = svc();
        let user_id = ObjectId::new();
        let pair = s.generate_tokens(user_id, "a@b.c", "u").unwrap();
        let err = s.verify_agent_token(&pair.access_token).unwrap_err();
        matches!(err, AuthError::InvalidToken(_));
    }

    #[test]
    fn enrollment_token_rejects_agent_token() {
        let s = svc();
        let agent_id = ObjectId::new();
        let tenant = ObjectId::new();
        let token = s.issue_agent_token(agent_id, tenant, Some(60)).unwrap();
        let err = s.verify_enrollment_token(&token).unwrap_err();
        matches!(err, AuthError::InvalidToken(_));
    }

    #[test]
    fn enrollment_tokens_have_unique_jti() {
        let s = svc();
        let admin = ObjectId::new();
        let tenant = ObjectId::new();
        let (_, jti1) = s.issue_enrollment_token(admin, tenant, 600).unwrap();
        let (_, jti2) = s.issue_enrollment_token(admin, tenant, 600).unwrap();
        assert_ne!(jti1, jti2);
    }

    // ─── tunnel-client + tunnel-enrollment audiences ──────────────────
    //
    // Plan §"What changed from v1" #6 — these audiences are NOT named
    // `Client` / `ClientEnrollment` because "Client" is overloaded
    // across the codebase. The matrix below locks every
    // verify-rejects-the-wrong-audience pair, in both directions, so a
    // leaked Agent token can't bootstrap a tunnel and a leaked
    // TunnelClient token can't drive an agent-side endpoint.

    #[test]
    fn tunnel_client_token_roundtrip() {
        let s = svc();
        let cid = ObjectId::new();
        let tid = ObjectId::new();
        let uid = ObjectId::new();
        let token = s
            .issue_tunnel_client_token(cid, tid, uid, Some(60))
            .unwrap();
        let claims = s.verify_tunnel_client_token(&token).unwrap();
        assert_eq!(claims.sub, cid.to_hex());
        assert_eq!(claims.tenant_id, tid.to_hex());
        assert_eq!(claims.owner_user_id, uid.to_hex());
        assert_eq!(claims.token_type, TokenType::TunnelClient);
    }

    #[test]
    fn tunnel_enrollment_token_roundtrip() {
        let s = svc();
        let admin = ObjectId::new();
        let tenant = ObjectId::new();
        let (token, jti) = s.issue_tunnel_enrollment_token(admin, tenant, 600).unwrap();
        let claims = s.verify_tunnel_enrollment_token(&token).unwrap();
        assert_eq!(claims.sub, admin.to_hex());
        assert_eq!(claims.tenant_id, tenant.to_hex());
        assert_eq!(claims.jti, jti);
        assert_eq!(claims.token_type, TokenType::TunnelEnrollment);
    }

    #[test]
    fn tunnel_enrollment_tokens_have_unique_jti() {
        let s = svc();
        let admin = ObjectId::new();
        let tenant = ObjectId::new();
        let (_, jti1) = s.issue_tunnel_enrollment_token(admin, tenant, 600).unwrap();
        let (_, jti2) = s.issue_tunnel_enrollment_token(admin, tenant, 600).unwrap();
        assert_ne!(jti1, jti2);
    }

    // verify_tunnel_client_token rejects every other audience
    #[test]
    fn tunnel_client_verify_rejects_access_token() {
        let s = svc();
        let pair = s.generate_tokens(ObjectId::new(), "a@b.c", "u").unwrap();
        let err = s
            .verify_tunnel_client_token(&pair.access_token)
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn tunnel_client_verify_rejects_refresh_token() {
        let s = svc();
        let pair = s.generate_tokens(ObjectId::new(), "a@b.c", "u").unwrap();
        let err = s
            .verify_tunnel_client_token(&pair.refresh_token)
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn tunnel_client_verify_rejects_agent_token() {
        let s = svc();
        let t = s
            .issue_agent_token(ObjectId::new(), ObjectId::new(), Some(60))
            .unwrap();
        let err = s.verify_tunnel_client_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn tunnel_client_verify_rejects_enrollment_token() {
        let s = svc();
        let (t, _) = s
            .issue_enrollment_token(ObjectId::new(), ObjectId::new(), 60)
            .unwrap();
        let err = s.verify_tunnel_client_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn tunnel_client_verify_rejects_tunnel_enrollment_token() {
        let s = svc();
        let (t, _) = s
            .issue_tunnel_enrollment_token(ObjectId::new(), ObjectId::new(), 60)
            .unwrap();
        let err = s.verify_tunnel_client_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    // verify_tunnel_enrollment_token rejects every other audience
    #[test]
    fn tunnel_enrollment_verify_rejects_access_token() {
        let s = svc();
        let pair = s.generate_tokens(ObjectId::new(), "a@b.c", "u").unwrap();
        let err = s
            .verify_tunnel_enrollment_token(&pair.access_token)
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn tunnel_enrollment_verify_rejects_refresh_token() {
        let s = svc();
        let pair = s.generate_tokens(ObjectId::new(), "a@b.c", "u").unwrap();
        let err = s
            .verify_tunnel_enrollment_token(&pair.refresh_token)
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn tunnel_enrollment_verify_rejects_agent_token() {
        let s = svc();
        let t = s
            .issue_agent_token(ObjectId::new(), ObjectId::new(), Some(60))
            .unwrap();
        let err = s.verify_tunnel_enrollment_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn tunnel_enrollment_verify_rejects_enrollment_token() {
        let s = svc();
        let (t, _) = s
            .issue_enrollment_token(ObjectId::new(), ObjectId::new(), 60)
            .unwrap();
        let err = s.verify_tunnel_enrollment_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn tunnel_enrollment_verify_rejects_tunnel_client_token() {
        let s = svc();
        let t = s
            .issue_tunnel_client_token(ObjectId::new(), ObjectId::new(), ObjectId::new(), Some(60))
            .unwrap();
        let err = s.verify_tunnel_enrollment_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    // Existing verifiers reject the new audiences (defence in depth —
    // a leaked TunnelClient must not unlock an agent's privileges).
    #[test]
    fn agent_verify_rejects_tunnel_client_token() {
        let s = svc();
        let t = s
            .issue_tunnel_client_token(ObjectId::new(), ObjectId::new(), ObjectId::new(), Some(60))
            .unwrap();
        let err = s.verify_agent_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn enrollment_verify_rejects_tunnel_enrollment_token() {
        let s = svc();
        let (t, _) = s
            .issue_tunnel_enrollment_token(ObjectId::new(), ObjectId::new(), 60)
            .unwrap();
        let err = s.verify_enrollment_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn access_verify_rejects_tunnel_client_token() {
        let s = svc();
        let t = s
            .issue_tunnel_client_token(ObjectId::new(), ObjectId::new(), ObjectId::new(), Some(60))
            .unwrap();
        let err = s.verify_access_token(&t).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }
}
