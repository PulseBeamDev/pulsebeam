use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use pulsebeam_auth::{api_key::ApiKey, keys::ApiSigningKey};
use pulsebeam_server::{Action, Grant, GrantSet, sign_participant_token};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let fixture = PathBuf::from(args.next().ok_or("missing fixture directory")?);
    let fanouts = args
        .map(|count| count.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()?;
    let id = |last| [0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, last];
    let api = ApiKey::new(id(1), id(2), [42; 32])?;
    let expiration = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 600;
    let token = |subject: &str, actions| -> Result<_, Box<dyn std::error::Error>> {
        let grants = GrantSet::new(vec![Grant::new(
            vec!["release-roster".into()],
            actions,
            None,
            None,
            None,
        )?])?;
        Ok(sign_participant_token(&api.to_secret_string(), &grants, subject, expiration)?.token)
    };

    let mut ca = CertificateParams::default();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate()?;
    fs::write(fixture.join("ca.pem"), ca.self_signed(&ca_key)?.pem())?;
    let issuer = Issuer::from_params(&ca, ca_key);
    let mut receipts = Vec::new();
    for node in 0..2 {
        let path = fixture.join(format!("node-{node}"));
        fs::create_dir(&path)?;
        let mut params = CertificateParams::new(vec![format!("node-{node}.smoke.test")])?;
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = KeyPair::generate()?;
        let certificate = params.signed_by(&key, &issuer)?;
        fs::write(path.join("tls.pem"), certificate.pem())?;
        fs::write(path.join("tls-key.pem"), key.serialize_pem())?;
        fs::write(path.join("tls.der"), certificate.der())?;
        let receipt = ApiSigningKey::from_seed([19 + node; 32]);
        fs::write(path.join("receipt.pkcs8"), receipt.to_secret_string())?;
        receipts.push(receipt.public_key());
    }
    let registry = json!({
        "projectId": api.project_id(),
        "keys": [{"keyId": api.key_id(), "publicKey": api.signing_key().public_key()}],
    });
    let waves = fanouts
        .into_iter()
        .enumerate()
        .map(|(wave, count)| {
            let publisher = format!("publisher-{wave}");
            let mut tokens = vec![token(&publisher, vec![Action::Join, Action::Publish])?];
            for viewer in 0..count {
                tokens.push(token(
                    &format!("viewer-{wave}-{viewer}"),
                    vec![Action::Join, Action::Subscribe],
                )?);
            }
            Ok::<_, Box<dyn std::error::Error>>(json!({"publisher": publisher, "tokens": tokens}))
        })
        .collect::<Result<Vec<_>, _>>()?;
    println!(
        "{}",
        json!({
            "registry": registry,
            "token": token("fixture", vec![Action::Join])?,
            "waves": waves,
            "receipts": receipts,
        })
    );
    Ok(())
}
