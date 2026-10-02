use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signer, SigningKey};
use pulsebeam_auth_conformance::{Cases, Golden, Vectors, decode_hex, encode_hex};
use pulsebeam_core::{
    auth::ApiSigningKey,
    identity::{ApiKeyId, ProjectId},
};
use serde::Serialize;
use std::{error::Error, path::PathBuf};

#[derive(Serialize)]
struct Header {
    alg: &'static str,
    kid: ApiKeyId,
    typ: &'static str,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: ProjectId,
    aud: &'static str,
    sub: &'a str,
    room: &'a str,
    exp: u64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let root =
        PathBuf::from(std::env::var_os("BUILD_WORKSPACE_DIRECTORY").ok_or("run through Bazel")?);
    let cases: Cases = serde_json::from_str(include_str!("../../cases.json"))?;
    let mut valid = Vec::with_capacity(cases.valid.len());
    for case in cases.valid {
        let input = case.input;
        let project: ProjectId = input.project_id.parse()?;
        let key: ApiKeyId = input.key_id.parse()?;
        let seed = decode_hex(&case.seed_hex)?;
        let supplied: ApiSigningKey = input.secret.parse()?;
        if supplied != ApiSigningKey::from_seed(seed) {
            return Err("fixture secret does not match seed".into());
        }
        let signing = SigningKey::from_bytes(&seed);
        let header = serde_json::to_string(&Header {
            alg: "EdDSA",
            kid: key,
            typ: "pb+jwt",
        })?;
        let claims = serde_json::to_string(&Claims {
            iss: project,
            aud: "pb",
            sub: &input.participant,
            room: &input.room,
            exp: input.expiration.parse()?,
        })?;
        let signing_input = format!(
            "{}.{}",
            BASE64URL_NOPAD.encode(header.as_bytes()),
            BASE64URL_NOPAD.encode(claims.as_bytes())
        );
        let signature = BASE64URL_NOPAD.encode(&signing.sign(signing_input.as_bytes()).to_bytes());
        let token = format!("{signing_input}.{signature}");
        valid.push(Golden {
            name: case.name,
            input,
            canonical_project_id: project.as_str(),
            canonical_key_id: key.as_str(),
            seed_hex: case.seed_hex,
            public_key_hex: encode_hex(signing.verifying_key().as_bytes()),
            header,
            claims,
            signing_input,
            signature,
            token,
        });
    }
    let vectors = Vectors {
        version: cases.version,
        base: cases.base,
        valid,
        invalid: cases.invalid,
    };
    std::fs::write(
        root.join("server/auth/vectors.json"),
        serde_json::to_string_pretty(&vectors)? + "\n",
    )?;
    Ok(())
}
