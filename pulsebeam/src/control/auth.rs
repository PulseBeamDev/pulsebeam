use std::collections::HashMap;
use std::time::Duration;

use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use pulsebeam_runtime::rand::RngCore;

use crate::entity::{
    ConnectionEpoch, ExternalRoomId, Identity, ParticipantId, encode_opaque_secret,
};

pub use crate::entity::KeyId;

const RESUME_TYP: &str = "pb-resume+jwt";
const MAX_LEEWAY: Duration = Duration::from_secs(300);

pub const DEFAULT_LEEWAY: Duration = Duration::from_secs(60);
pub const DEFAULT_MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);
pub const DEFAULT_RESUME_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum AuthError {
    #[error("authorization required")]
    MissingToken,
    #[error("token is malformed")]
    MalformedToken,
    #[error("unknown key id")]
    UnknownKid,
    #[error("signature verification failed")]
    InvalidSignature,
    #[error("token has expired")]
    TokenExpired,
    #[error("token is not yet valid")]
    TokenNotYetValid,
    #[error("token audience is not accepted")]
    InvalidAudience,
    #[error("token issuer is not accepted")]
    InvalidIssuer,
    #[error("resume token is invalid")]
    InvalidResumeToken,
    #[error("resume token has expired")]
    ResumeTokenExpired,
    #[error("unknown resume key id")]
    UnknownResumeKid,
    #[error("token is not valid for this room")]
    RoomMismatch,
    #[error("token subject does not own this participant")]
    SubjectMismatch,
    #[error("resume token is for a different participant")]
    ParticipantMismatch,
    #[error("authentication is not configured on this node")]
    NotConfigured,
}

impl AuthError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingToken => "missing_token",
            Self::MalformedToken => "malformed_token",
            Self::UnknownKid => "unknown_kid",
            Self::InvalidSignature => "invalid_signature",
            Self::TokenExpired => "token_expired",
            Self::TokenNotYetValid => "token_not_yet_valid",
            Self::InvalidAudience => "invalid_audience",
            Self::InvalidIssuer => "invalid_issuer",
            Self::InvalidResumeToken => "invalid_resume_token",
            Self::ResumeTokenExpired => "resume_token_expired",
            Self::UnknownResumeKid => "unknown_resume_kid",
            Self::RoomMismatch => "room_mismatch",
            Self::SubjectMismatch => "subject_mismatch",
            Self::ParticipantMismatch => "participant_mismatch",
            Self::NotConfigured => "auth_not_configured",
        }
    }

    /// A valid token that does not authorize this particular request is a 403, not a 401:
    /// re-authenticating would not help.
    pub fn is_forbidden(&self) -> bool {
        matches!(
            self,
            Self::RoomMismatch | Self::SubjectMismatch | Self::ParticipantMismatch
        )
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::NotConfigured)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtAlg {
    Ed25519,
    Es256,
}

impl JwtAlg {
    fn algorithm(self) -> Algorithm {
        match self {
            Self::Ed25519 => Algorithm::EdDSA,
            Self::Es256 => Algorithm::ES256,
        }
    }
}

/// Public key material for verifying access tokens.
///
/// Despite their names, `DecodingKey::from_ed_der`/`from_ec_der` store the bytes verbatim and hand
/// them to aws-lc's `verify_sig`, which wants a raw Ed25519 key or an uncompressed P-256 point --
/// not SPKI. `Pem` is the SPKI-shaped path.
#[derive(Debug, Clone)]
pub enum JwtKeyBytes {
    Ed25519Raw(Vec<u8>),
    Es256Raw(Vec<u8>),
    Pem(String),
}

#[derive(Clone)]
pub struct VerifyingKey {
    alg: JwtAlg,
    key: DecodingKey,
}

impl VerifyingKey {
    pub fn new(alg: JwtAlg, bytes: JwtKeyBytes) -> Result<Self, AuthError> {
        let key = match (alg, &bytes) {
            (JwtAlg::Ed25519, JwtKeyBytes::Ed25519Raw(b)) => {
                if b.len() != 32 {
                    return Err(AuthError::MalformedToken);
                }
                DecodingKey::from_ed_der(b)
            }
            (JwtAlg::Es256, JwtKeyBytes::Es256Raw(b)) => {
                if b.len() != 65 || b[0] != 0x04 {
                    return Err(AuthError::MalformedToken);
                }
                DecodingKey::from_ec_der(b)
            }
            (JwtAlg::Ed25519, JwtKeyBytes::Pem(p)) => {
                DecodingKey::from_ed_pem(p.as_bytes()).map_err(|_| AuthError::MalformedToken)?
            }
            (JwtAlg::Es256, JwtKeyBytes::Pem(p)) => {
                DecodingKey::from_ec_pem(p.as_bytes()).map_err(|_| AuthError::MalformedToken)?
            }
            _ => return Err(AuthError::MalformedToken),
        };
        Ok(Self { alg, key })
    }
}

impl std::fmt::Debug for VerifyingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyingKey")
            .field("alg", &self.alg)
            .finish_non_exhaustive()
    }
}

/// Cluster-wide HMAC keys for resume tokens. The first entry signs; every entry verifies, so a key
/// can be rotated in and the old one drained before removal.
#[derive(Clone)]
pub struct ResumeKeyring {
    keys: Vec<(KeyId, [u8; 32])>,
}

impl ResumeKeyring {
    pub fn new(keys: Vec<(KeyId, [u8; 32])>) -> Option<Self> {
        if keys.is_empty() || keys.iter().any(|(_, secret)| secret == &[0u8; 32]) {
            return None;
        }
        Some(Self { keys })
    }

    /// A single random key. Resume tokens minted under it do not survive a restart, which is why
    /// callers are expected to warn.
    pub fn ephemeral(rng: &mut impl RngCore) -> Self {
        let mut secret = [0u8; 32];
        rng.fill_bytes(&mut secret);
        // A random 32-byte secret hitting all-zero would silently disable the keyring guard above.
        debug_assert_ne!(secret, [0u8; 32]);
        let kid = KeyId::new(&encode_opaque_secret(&secret[..8]))
            .expect("base32 of 8 bytes is a valid key id");
        Self {
            keys: vec![(kid, secret)],
        }
    }

    fn signing(&self) -> &(KeyId, [u8; 32]) {
        debug_assert!(!self.keys.is_empty(), "resume keyring must never be empty");
        &self.keys[0]
    }

    fn lookup(&self, kid: &str) -> Option<&[u8; 32]> {
        self.keys
            .iter()
            .find(|(k, _)| k.as_str() == kid)
            .map(|(_, secret)| secret)
    }
}

impl std::fmt::Debug for ResumeKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeKeyring")
            .field(
                "kids",
                &self.keys.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
pub struct Capabilities {
    pub publish: bool,
    pub subscribe: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            publish: true,
            subscribe: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PulseBeamClaims {
    /// Must equal the `{external_room_id}` path segment exactly. No wildcards.
    pub room: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_true")]
    pub publish: bool,
    #[serde(default = "default_true")]
    pub subscribe: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_secs: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AccessClaims {
    pub iss: String,
    pub sub: Identity,
    pub aud: String,
    pub exp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    pub jti: String,
    pub pb: PulseBeamClaims,
}

impl AccessClaims {
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            publish: self.pb.publish,
            subscribe: self.pb.subscribe,
        }
    }

    /// When the application's grant runs out, independent of any single token's `exp`.
    pub fn session_expires_at(&self, now: i64) -> i64 {
        match self.pb.max_duration_secs {
            Some(max) => self
                .exp
                .min(self.iat.unwrap_or(now).saturating_add(max as i64)),
            None => self.exp,
        }
    }
}

/// Proves ownership of a `ParticipantId` in a room, and nothing else.
///
/// Capabilities and display name are deliberately absent: they are application-owned and arrive on
/// the fresh access token presented alongside this one, so caching them here could only produce a
/// stale snapshot that out-votes the application's current decision.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResumeClaims {
    pub iss: String,
    pub aud: String,
    pub sub: Identity,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
    pub rid: String,
    pub pid: ParticipantId,
    pub epoch: ConnectionEpoch,
}

#[derive(Clone)]
pub struct AuthConfig {
    access_keys: HashMap<KeyId, VerifyingKey>,
    audiences: Vec<String>,
    issuers: Vec<String>,
    leeway: Duration,
    max_token_lifetime: Duration,
    resume_keys: ResumeKeyring,
    resume_ttl: Duration,
    /// `iss` stamped onto resume tokens this cluster mints.
    resume_issuer: String,
}

pub struct AuthConfigBuilder {
    access_keys: HashMap<KeyId, VerifyingKey>,
    audiences: Vec<String>,
    issuers: Vec<String>,
    leeway: Duration,
    max_token_lifetime: Duration,
    resume_keys: Option<ResumeKeyring>,
    resume_ttl: Duration,
    resume_issuer: String,
}

impl AuthConfigBuilder {
    pub fn new() -> Self {
        Self {
            access_keys: HashMap::new(),
            audiences: Vec::new(),
            issuers: Vec::new(),
            leeway: DEFAULT_LEEWAY,
            max_token_lifetime: DEFAULT_MAX_TOKEN_LIFETIME,
            resume_keys: None,
            resume_ttl: DEFAULT_RESUME_TTL,
            resume_issuer: "pulsebeam".to_string(),
        }
    }

    pub fn access_key(mut self, kid: KeyId, key: VerifyingKey) -> Self {
        self.access_keys.insert(kid, key);
        self
    }

    pub fn audience(mut self, aud: impl Into<String>) -> Self {
        self.audiences.push(aud.into());
        self
    }

    pub fn issuer(mut self, iss: impl Into<String>) -> Self {
        self.issuers.push(iss.into());
        self
    }

    pub fn leeway(mut self, leeway: Duration) -> Self {
        self.leeway = leeway;
        self
    }

    pub fn max_token_lifetime(mut self, lifetime: Duration) -> Self {
        self.max_token_lifetime = lifetime;
        self
    }

    pub fn resume_keys(mut self, keyring: ResumeKeyring) -> Self {
        self.resume_keys = Some(keyring);
        self
    }

    pub fn resume_ttl(mut self, ttl: Duration) -> Self {
        self.resume_ttl = ttl;
        self
    }

    pub fn resume_issuer(mut self, iss: impl Into<String>) -> Self {
        self.resume_issuer = iss.into();
        self
    }

    /// Returns `None` when the result could not fail closed: no access key, no audience (the
    /// audience check is what stops a token minted for another service from being replayed here),
    /// or no resume key.
    pub fn build(self, rng: &mut impl RngCore) -> Option<AuthConfig> {
        if self.access_keys.is_empty() || self.audiences.is_empty() {
            return None;
        }
        debug_assert!(
            self.leeway <= MAX_LEEWAY,
            "auth leeway {:?} is implausibly large",
            self.leeway
        );
        debug_assert!(self.resume_ttl > Duration::ZERO);
        let resume_keys = self
            .resume_keys
            .unwrap_or_else(|| ResumeKeyring::ephemeral(rng));

        Some(AuthConfig {
            access_keys: self.access_keys,
            audiences: self.audiences,
            issuers: self.issuers,
            leeway: self.leeway.min(MAX_LEEWAY),
            max_token_lifetime: self.max_token_lifetime,
            resume_keys,
            resume_ttl: self.resume_ttl,
            resume_issuer: self.resume_issuer,
        })
    }
}

impl Default for AuthConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthConfig {
    pub fn builder() -> AuthConfigBuilder {
        AuthConfigBuilder::new()
    }

    pub fn resume_ttl(&self) -> Duration {
        self.resume_ttl
    }

    fn leeway_secs(&self) -> i64 {
        self.leeway.as_secs() as i64
    }

    /// `exp`/`nbf` are checked here rather than inside `jsonwebtoken` so that every time-dependent
    /// decision comes from the caller's clock. The simulator runs on turmoil's virtual clock, where
    /// a wall-clock check inside the library would be both wrong and untestable.
    fn check_time(&self, now: i64, exp: i64, nbf: Option<i64>) -> Result<(), AuthError> {
        let leeway = self.leeway_secs();
        if now.saturating_sub(leeway) >= exp {
            return Err(AuthError::TokenExpired);
        }
        if let Some(nbf) = nbf
            && now.saturating_add(leeway) < nbf
        {
            return Err(AuthError::TokenNotYetValid);
        }
        Ok(())
    }

    fn validation_for(&self, alg: JwtAlg) -> Validation {
        // The algorithm comes from the configured key, never from the token, and the allowlist has
        // exactly one entry. This is what makes `alg: none` and alg-confusion unreachable.
        let mut validation = Validation::new(alg.algorithm());
        validation.leeway = self.leeway.as_secs();
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = true;
        validation.set_audience(&self.audiences);
        validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
        if !self.issuers.is_empty() {
            validation.set_issuer(&self.issuers);
        }
        validation
    }

    /// Verifies an access token and binds it to `room`.
    pub fn verify_access(
        &self,
        bearer: &str,
        room: &ExternalRoomId,
        now: i64,
    ) -> Result<AccessClaims, AuthError> {
        let token = bearer.trim();
        if token.is_empty() {
            return Err(AuthError::MissingToken);
        }

        let header = decode_header(token).map_err(|_| AuthError::MalformedToken)?;
        let kid = header.kid.as_deref().ok_or(AuthError::UnknownKid)?;
        let kid = KeyId::new(kid).map_err(|_| AuthError::UnknownKid)?;
        let key = self.access_keys.get(&kid).ok_or(AuthError::UnknownKid)?;

        let data = decode::<AccessClaims>(token, &key.key, &self.validation_for(key.alg))
            .map_err(map_jwt_error)?;
        let claims = data.claims;

        self.check_time(now, claims.exp, claims.nbf)?;

        if let Some(iat) = claims.iat
            && claims.exp.saturating_sub(iat) > self.max_token_lifetime.as_secs() as i64
        {
            return Err(AuthError::TokenExpired);
        }

        let claimed_room =
            ExternalRoomId::new(&claims.pb.room).map_err(|_| AuthError::RoomMismatch)?;
        if &claimed_room != room {
            return Err(AuthError::RoomMismatch);
        }

        debug_assert!(claims.exp > now - self.leeway_secs());
        debug_assert!(claims.nbf.is_none_or(|nbf| nbf <= claims.exp));
        Ok(claims)
    }

    pub fn mint_resume(
        &self,
        access: &AccessClaims,
        room: &ExternalRoomId,
        participant_id: ParticipantId,
        epoch: ConnectionEpoch,
        now: i64,
        rng: &mut impl RngCore,
    ) -> Result<(String, i64), AuthError> {
        let (kid, secret) = self.resume_keys.signing();
        let exp = now.saturating_add(self.resume_ttl.as_secs() as i64);

        let mut jti_bytes = [0u8; 16];
        rng.fill_bytes(&mut jti_bytes);

        let claims = ResumeClaims {
            iss: self.resume_issuer.clone(),
            aud: self.audiences[0].clone(),
            sub: access.sub.clone(),
            iat: now,
            exp,
            jti: encode_opaque_secret(&jti_bytes),
            rid: room.as_str().to_string(),
            pid: participant_id,
            epoch,
        };

        debug_assert!(claims.exp > claims.iat);
        debug_assert_eq!(claims.rid, room.as_str());

        let mut header = Header::new(Algorithm::HS256);
        header.typ = Some(RESUME_TYP.to_string());
        header.kid = Some(kid.as_str().to_string());

        let token = encode(&header, &claims, &EncodingKey::from_secret(secret))
            .map_err(|_| AuthError::InvalidResumeToken)?;
        Ok((token, exp))
    }

    /// Verifies a resume token and binds it to the room, the participant in the path, and the
    /// subject of the *fresh* access token presented alongside it.
    pub fn verify_resume(
        &self,
        token: &str,
        room: &ExternalRoomId,
        participant_id: &ParticipantId,
        access: &AccessClaims,
        now: i64,
    ) -> Result<ResumeClaims, AuthError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(AuthError::InvalidResumeToken);
        }

        let header = decode_header(token).map_err(|_| AuthError::InvalidResumeToken)?;
        // Distinct `typ` plus distinct key material: an access token can never verify here, and a
        // resume token can never verify as an access token.
        if header.typ.as_deref() != Some(RESUME_TYP) {
            return Err(AuthError::InvalidResumeToken);
        }
        let kid = header.kid.as_deref().ok_or(AuthError::UnknownResumeKid)?;
        let secret = self
            .resume_keys
            .lookup(kid)
            .ok_or(AuthError::UnknownResumeKid)?;

        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = self.leeway.as_secs();
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = true;
        validation.set_audience(&self.audiences);
        validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);

        let data = decode::<ResumeClaims>(token, &DecodingKey::from_secret(secret), &validation)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::InvalidAudience => AuthError::InvalidAudience,
                _ => AuthError::InvalidResumeToken,
            })?;
        let claims = data.claims;

        if self.check_time(now, claims.exp, None).is_err() {
            return Err(AuthError::ResumeTokenExpired);
        }

        if claims.rid != room.as_str() {
            return Err(AuthError::RoomMismatch);
        }
        if &claims.pid != participant_id {
            return Err(AuthError::ParticipantMismatch);
        }
        if claims.sub != access.sub {
            return Err(AuthError::SubjectMismatch);
        }

        debug_assert_eq!(header.typ.as_deref(), Some(RESUME_TYP));
        debug_assert_eq!(claims.rid, room.as_str());
        Ok(claims)
    }
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("kids", &self.access_keys.keys().collect::<Vec<_>>())
            .field("audiences", &self.audiences)
            .field("issuers", &self.issuers)
            .field("leeway", &self.leeway)
            .field("resume_keys", &self.resume_keys)
            .field("resume_ttl", &self.resume_ttl)
            .finish()
    }
}

fn map_jwt_error(e: jsonwebtoken::errors::Error) -> AuthError {
    use jsonwebtoken::errors::ErrorKind;
    match e.kind() {
        ErrorKind::InvalidSignature => AuthError::InvalidSignature,
        ErrorKind::InvalidAudience => AuthError::InvalidAudience,
        ErrorKind::InvalidIssuer => AuthError::InvalidIssuer,
        ErrorKind::ExpiredSignature => AuthError::TokenExpired,
        ErrorKind::ImmatureSignature => AuthError::TokenNotYetValid,
        // An algorithm the configured key does not permit is a forged token, not a malformed one.
        ErrorKind::InvalidAlgorithm | ErrorKind::InvalidKeyFormat => AuthError::InvalidSignature,
        _ => AuthError::MalformedToken,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_runtime::rand::seeded_rng;

    const NOW: i64 = 1_800_000_000;
    const AUD: &str = "pulsebeam-test";
    const ISS: &str = "https://app.example.com";
    const KID: &str = "key-2026-08";

    /// Deterministic Ed25519 pkcs8 keypair, generated once and pinned so the suite never touches
    /// OS entropy and a failure always reproduces.
    fn ed25519_pkcs8() -> Vec<u8> {
        let seed = [7u8; 32];
        // pkcs8 v1 wrapper around a raw Ed25519 seed.
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend_from_slice(&seed);
        der
    }

    /// The public key for the seed above, pinned rather than derived so the suite needs no
    /// crypto dependency of its own. `signing_and_verifying_keys_agree` proves they still match.
    const ED25519_PUBLIC: [u8; 32] = [
        234, 74, 108, 99, 226, 156, 82, 10, 190, 245, 80, 123, 19, 46, 197, 249, 149, 71, 118, 174,
        190, 190, 123, 146, 66, 30, 234, 105, 20, 70, 210, 44,
    ];

    fn ed25519_keys() -> (EncodingKey, Vec<u8>) {
        (
            EncodingKey::from_ed_der(&ed25519_pkcs8()),
            ED25519_PUBLIC.to_vec(),
        )
    }

    fn rng() -> impl RngCore {
        seeded_rng(42)
    }

    fn config() -> AuthConfig {
        let (_, public) = ed25519_keys();
        AuthConfig::builder()
            .access_key(
                KeyId::new(KID).unwrap(),
                VerifyingKey::new(JwtAlg::Ed25519, JwtKeyBytes::Ed25519Raw(public)).unwrap(),
            )
            .audience(AUD)
            .issuer(ISS)
            .resume_keys(
                ResumeKeyring::new(vec![(KeyId::new("rk-1").unwrap(), [3u8; 32])]).unwrap(),
            )
            .build(&mut rng())
            .expect("config must be buildable")
    }

    fn room() -> ExternalRoomId {
        ExternalRoomId::new("standup").unwrap()
    }

    fn claims() -> AccessClaims {
        AccessClaims {
            iss: ISS.to_string(),
            sub: Identity::new("user_1042").unwrap(),
            aud: AUD.to_string(),
            exp: NOW + 3600,
            nbf: None,
            iat: Some(NOW),
            jti: "jti-1".to_string(),
            pb: PulseBeamClaims {
                room: "standup".to_string(),
                name: Some("Ada".to_string()),
                publish: true,
                subscribe: true,
                max_duration_secs: None,
            },
        }
    }

    fn sign(claims: &AccessClaims) -> String {
        let (key, _) = ed25519_keys();
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(KID.to_string());
        encode(&header, claims, &key).unwrap()
    }

    fn participant() -> ParticipantId {
        ParticipantId::new(&mut rng())
    }

    #[test]
    fn signing_and_verifying_keys_agree() {
        // If the pinned public key ever drifts from the pinned seed, every other test in this
        // module would fail confusingly; fail here instead, with the reason.
        let cfg = config();
        assert!(
            cfg.verify_access(&sign(&claims()), &room(), NOW).is_ok(),
            "ED25519_PUBLIC no longer matches the pinned pkcs8 seed"
        );
    }

    #[test]
    fn a_well_formed_token_is_accepted_and_bound_to_its_room() {
        let verified = config()
            .verify_access(&sign(&claims()), &room(), NOW)
            .unwrap();
        assert_eq!(verified.sub.as_str(), "user_1042");
        assert_eq!(
            verified.capabilities(),
            Capabilities {
                publish: true,
                subscribe: true
            }
        );
    }

    #[test]
    fn a_token_for_another_room_is_forbidden_not_unauthorized() {
        let other = ExternalRoomId::new("retro").unwrap();
        let err = config()
            .verify_access(&sign(&claims()), &other, NOW)
            .unwrap_err();
        assert_eq!(err, AuthError::RoomMismatch);
        assert!(
            err.is_forbidden(),
            "a valid token on the wrong room is a 403"
        );
    }

    #[test]
    fn expiry_and_nbf_are_evaluated_against_the_supplied_clock() {
        let cfg = config();
        let token = sign(&claims());
        // Valid now, expired well after exp, both decided by the caller's clock rather than
        // the wall clock -- the property the simulator's virtual time depends on.
        assert!(cfg.verify_access(&token, &room(), NOW).is_ok());
        assert_eq!(
            cfg.verify_access(&token, &room(), NOW + 7200).unwrap_err(),
            AuthError::TokenExpired
        );

        let mut future = claims();
        future.nbf = Some(NOW + 3000);
        assert_eq!(
            cfg.verify_access(&sign(&future), &room(), NOW).unwrap_err(),
            AuthError::TokenNotYetValid
        );
    }

    #[test]
    fn leeway_is_applied_on_both_edges() {
        let cfg = config();
        let mut expiring = claims();
        expiring.exp = NOW;
        // Within leeway of exp, so still accepted.
        assert!(
            cfg.verify_access(&sign(&expiring), &room(), NOW + 30)
                .is_ok()
        );
        assert_eq!(
            cfg.verify_access(&sign(&expiring), &room(), NOW + 90)
                .unwrap_err(),
            AuthError::TokenExpired
        );
    }

    #[test]
    fn wrong_audience_and_issuer_are_rejected() {
        let cfg = config();
        let mut wrong_aud = claims();
        wrong_aud.aud = "someone-else".to_string();
        assert_eq!(
            cfg.verify_access(&sign(&wrong_aud), &room(), NOW)
                .unwrap_err(),
            AuthError::InvalidAudience
        );

        let mut wrong_iss = claims();
        wrong_iss.iss = "https://evil.example.com".to_string();
        assert_eq!(
            cfg.verify_access(&sign(&wrong_iss), &room(), NOW)
                .unwrap_err(),
            AuthError::InvalidIssuer
        );
    }

    #[test]
    fn an_unknown_or_missing_kid_is_rejected() {
        let cfg = config();
        let (key, _) = ed25519_keys();

        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some("key-does-not-exist".to_string());
        let token = encode(&header, &claims(), &key).unwrap();
        assert_eq!(
            cfg.verify_access(&token, &room(), NOW).unwrap_err(),
            AuthError::UnknownKid
        );

        let bare = encode(&Header::new(Algorithm::EdDSA), &claims(), &key).unwrap();
        assert_eq!(
            cfg.verify_access(&bare, &room(), NOW).unwrap_err(),
            AuthError::UnknownKid
        );
    }

    #[test]
    fn alg_none_is_rejected() {
        let cfg = config();
        // `alg: none` with an empty signature, hand-assembled: jsonwebtoken will not mint one.
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(format!(r#"{{"alg":"none","kid":"{KID}"}}"#));
        let payload = b64.encode(serde_json::to_vec(&claims()).unwrap());
        let token = format!("{header}.{payload}.");
        assert!(cfg.verify_access(&token, &room(), NOW).is_err());
    }

    #[test]
    fn a_token_hmac_signed_with_the_public_key_is_rejected() {
        // The classic algorithm-confusion attack: the attacker knows the Ed25519 public key and
        // signs HS256 with those bytes, hoping the verifier trusts the token's own `alg`.
        let cfg = config();
        let (_, public) = ed25519_keys();
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(KID.to_string());
        let forged = encode(&header, &claims(), &EncodingKey::from_secret(&public)).unwrap();
        assert_eq!(
            cfg.verify_access(&forged, &room(), NOW).unwrap_err(),
            AuthError::InvalidSignature
        );
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let cfg = config();
        let token = sign(&claims());
        let mut parts: Vec<&str> = token.split('.').collect();
        let mut escalated = claims();
        escalated.pb.room = "retro".to_string();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = b64.encode(serde_json::to_vec(&escalated).unwrap());
        parts[1] = &payload;
        let tampered = parts.join(".");
        assert_eq!(
            cfg.verify_access(&tampered, &room(), NOW).unwrap_err(),
            AuthError::InvalidSignature
        );
    }

    #[test]
    fn garbage_and_empty_tokens_are_rejected_without_panicking() {
        let cfg = config();
        for garbage in [
            "",
            "   ",
            "not-a-token",
            "a.b.c",
            "...",
            "\0",
            &"x".repeat(10_000),
        ] {
            let err = cfg.verify_access(garbage, &room(), NOW).unwrap_err();
            assert!(
                !err.is_forbidden(),
                "{garbage:?} must not read as authorized"
            );
        }
    }

    #[test]
    fn a_resume_token_round_trips_and_preserves_the_participant() {
        let cfg = config();
        let access = cfg.verify_access(&sign(&claims()), &room(), NOW).unwrap();
        let pid = participant();
        let (token, exp) = cfg
            .mint_resume(
                &access,
                &room(),
                pid,
                ConnectionEpoch::new(3),
                NOW,
                &mut rng(),
            )
            .unwrap();
        assert_eq!(exp, NOW + DEFAULT_RESUME_TTL.as_secs() as i64);

        let resumed = cfg
            .verify_resume(&token, &room(), &pid, &access, NOW + 60)
            .unwrap();
        assert_eq!(
            resumed.pid, pid,
            "the participant id is what makes TrackIds stable"
        );
        assert_eq!(resumed.epoch, ConnectionEpoch::new(3));
        assert_eq!(resumed.sub, access.sub);
    }

    #[test]
    fn a_resume_token_outlives_the_access_token_that_minted_it() {
        // The point of pairing a resume token with a *fresh* JWT: a long session must survive its
        // original token expiring.
        let cfg = config();
        let access = cfg.verify_access(&sign(&claims()), &room(), NOW).unwrap();
        let pid = participant();
        let (token, _) = cfg
            .mint_resume(
                &access,
                &room(),
                pid,
                ConnectionEpoch::ZERO,
                NOW,
                &mut rng(),
            )
            .unwrap();

        let later = NOW + 3000;
        let mut refreshed = claims();
        refreshed.exp = later + 3600;
        refreshed.iat = Some(later);
        let fresh = cfg
            .verify_access(&sign(&refreshed), &room(), later)
            .unwrap();

        assert!(
            cfg.verify_resume(&token, &room(), &pid, &fresh, later)
                .is_ok()
        );
    }

    #[test]
    fn a_resume_token_is_bound_to_room_participant_and_subject() {
        let cfg = config();
        let access = cfg.verify_access(&sign(&claims()), &room(), NOW).unwrap();
        let pid = participant();
        let (token, _) = cfg
            .mint_resume(
                &access,
                &room(),
                pid,
                ConnectionEpoch::ZERO,
                NOW,
                &mut rng(),
            )
            .unwrap();

        let other_room = ExternalRoomId::new("retro").unwrap();
        assert_eq!(
            cfg.verify_resume(&token, &other_room, &pid, &access, NOW)
                .unwrap_err(),
            AuthError::RoomMismatch
        );

        let other_pid = ParticipantId::new(&mut seeded_rng(99));
        assert_eq!(
            cfg.verify_resume(&token, &room(), &other_pid, &access, NOW)
                .unwrap_err(),
            AuthError::ParticipantMismatch
        );

        // Another user's perfectly valid token must not adopt this participant.
        let mut intruder_claims = claims();
        intruder_claims.sub = Identity::new("user_9999").unwrap();
        let intruder = cfg
            .verify_access(&sign(&intruder_claims), &room(), NOW)
            .unwrap();
        assert_eq!(
            cfg.verify_resume(&token, &room(), &pid, &intruder, NOW)
                .unwrap_err(),
            AuthError::SubjectMismatch
        );
    }

    #[test]
    fn an_expired_resume_token_is_rejected() {
        let cfg = config();
        let access = cfg.verify_access(&sign(&claims()), &room(), NOW).unwrap();
        let pid = participant();
        let (token, exp) = cfg
            .mint_resume(
                &access,
                &room(),
                pid,
                ConnectionEpoch::ZERO,
                NOW,
                &mut rng(),
            )
            .unwrap();
        assert_eq!(
            cfg.verify_resume(&token, &room(), &pid, &access, exp + 3600)
                .unwrap_err(),
            AuthError::ResumeTokenExpired
        );
    }

    #[test]
    fn the_two_token_families_are_not_interchangeable() {
        let cfg = config();
        let access_token = sign(&claims());
        let access = cfg.verify_access(&access_token, &room(), NOW).unwrap();
        let pid = participant();
        let (resume_token, _) = cfg
            .mint_resume(
                &access,
                &room(),
                pid,
                ConnectionEpoch::ZERO,
                NOW,
                &mut rng(),
            )
            .unwrap();

        // An access token presented where a resume token belongs.
        assert!(
            cfg.verify_resume(&access_token, &room(), &pid, &access, NOW)
                .is_err()
        );
        // A resume token presented as a bearer credential.
        assert!(cfg.verify_access(&resume_token, &room(), NOW).is_err());
    }

    #[test]
    fn a_resume_token_verifies_under_a_rotated_keyring_until_its_key_is_retired() {
        let (_, public) = ed25519_keys();
        let old = (KeyId::new("rk-1").unwrap(), [3u8; 32]);
        let new = (KeyId::new("rk-2").unwrap(), [9u8; 32]);

        let build = |keys: Vec<(KeyId, [u8; 32])>| {
            AuthConfig::builder()
                .access_key(
                    KeyId::new(KID).unwrap(),
                    VerifyingKey::new(JwtAlg::Ed25519, JwtKeyBytes::Ed25519Raw(public.clone()))
                        .unwrap(),
                )
                .audience(AUD)
                .resume_keys(ResumeKeyring::new(keys).unwrap())
                .build(&mut rng())
                .unwrap()
        };

        let before = build(vec![old.clone()]);
        let access = before
            .verify_access(&sign(&claims()), &room(), NOW)
            .unwrap();
        let pid = participant();
        let (token, _) = before
            .mint_resume(
                &access,
                &room(),
                pid,
                ConnectionEpoch::ZERO,
                NOW,
                &mut rng(),
            )
            .unwrap();

        // Rotated in: the new key signs, the old one still verifies.
        let during = build(vec![new.clone(), old]);
        assert!(
            during
                .verify_resume(&token, &room(), &pid, &access, NOW)
                .is_ok()
        );

        // Old key retired: previously issued tokens stop working, by design.
        let after = build(vec![new]);
        assert_eq!(
            after
                .verify_resume(&token, &room(), &pid, &access, NOW)
                .unwrap_err(),
            AuthError::UnknownResumeKid
        );
    }

    #[test]
    fn a_config_that_could_not_fail_closed_is_refused() {
        let (_, public) = ed25519_keys();
        let key =
            || VerifyingKey::new(JwtAlg::Ed25519, JwtKeyBytes::Ed25519Raw(public.clone())).unwrap();

        // No key at all.
        assert!(
            AuthConfig::builder()
                .audience(AUD)
                .build(&mut rng())
                .is_none()
        );
        // No audience: without it, a token minted for a different service would verify here.
        assert!(
            AuthConfig::builder()
                .access_key(KeyId::new(KID).unwrap(), key())
                .build(&mut rng())
                .is_none()
        );
        // An all-zero resume secret is not a key.
        assert!(ResumeKeyring::new(vec![(KeyId::new("rk-1").unwrap(), [0u8; 32])]).is_none());
        assert!(ResumeKeyring::new(vec![]).is_none());
    }

    #[test]
    fn malformed_key_material_is_refused_at_configuration_time() {
        assert!(
            VerifyingKey::new(JwtAlg::Ed25519, JwtKeyBytes::Ed25519Raw(vec![1, 2, 3])).is_err()
        );
        assert!(VerifyingKey::new(JwtAlg::Es256, JwtKeyBytes::Es256Raw(vec![0x04; 10])).is_err());
        // An uncompressed point must carry the 0x04 tag.
        assert!(VerifyingKey::new(JwtAlg::Es256, JwtKeyBytes::Es256Raw(vec![0x02; 65])).is_err());
        assert!(VerifyingKey::new(JwtAlg::Ed25519, JwtKeyBytes::Pem("not a pem".into())).is_err());
    }

    #[test]
    fn max_duration_bounds_the_session_independently_of_exp() {
        let mut c = claims();
        c.pb.max_duration_secs = Some(600);
        let verified = config().verify_access(&sign(&c), &room(), NOW).unwrap();
        assert_eq!(verified.session_expires_at(NOW), NOW + 600);

        let plain = config()
            .verify_access(&sign(&claims()), &room(), NOW)
            .unwrap();
        assert_eq!(plain.session_expires_at(NOW), NOW + 3600);
    }

    #[test]
    fn an_implausibly_long_lived_token_is_rejected() {
        let cfg = config();
        let mut c = claims();
        c.iat = Some(NOW);
        c.exp = NOW + 86_400 * 30;
        assert_eq!(
            cfg.verify_access(&sign(&c), &room(), NOW).unwrap_err(),
            AuthError::TokenExpired
        );
    }

    #[test]
    fn capabilities_default_to_granted_but_are_honoured_when_withheld() {
        let cfg = config();
        let mut c = claims();
        c.pb.publish = false;
        let verified = cfg.verify_access(&sign(&c), &room(), NOW).unwrap();
        assert_eq!(
            verified.capabilities(),
            Capabilities {
                publish: false,
                subscribe: true
            }
        );
    }
}
