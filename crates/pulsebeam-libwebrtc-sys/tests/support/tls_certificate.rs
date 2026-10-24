//! Ephemeral real CA/server credentials for local TLS integration fixtures.
use std::{fs, path::Path};

pub fn openssl(args: &[&str]) {
    let output = crate::tools::command("openssl").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn certificate(directory: &Path) -> (String, Vec<u8>, Vec<u8>) {
    let ca = directory.join("ca.pem");
    let ca_key = directory.join("ca.key");
    let cert = directory.join("server.pem");
    let key = directory.join("server.key");
    let request = directory.join("server.csr");
    let extensions = directory.join("server.ext");
    fs::write(&extensions, "subjectAltName=DNS:relay.test\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n").unwrap();
    openssl(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-sha256",
        "-days",
        "1",
        "-keyout",
        ca_key.to_str().unwrap(),
        "-out",
        ca.to_str().unwrap(),
        "-subj",
        "/CN=Controlled TURN CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
    ]);
    openssl(&[
        "req",
        "-new",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        key.to_str().unwrap(),
        "-out",
        request.to_str().unwrap(),
        "-subj",
        "/CN=relay.test",
    ]);
    openssl(&[
        "x509",
        "-req",
        "-in",
        request.to_str().unwrap(),
        "-CA",
        ca.to_str().unwrap(),
        "-CAkey",
        ca_key.to_str().unwrap(),
        "-CAcreateserial",
        "-out",
        cert.to_str().unwrap(),
        "-days",
        "1",
        "-sha256",
        "-extfile",
        extensions.to_str().unwrap(),
    ]);
    let cert_der = directory.join("server.der");
    let key_der = directory.join("server-key.der");
    openssl(&[
        "x509",
        "-in",
        cert.to_str().unwrap(),
        "-outform",
        "DER",
        "-out",
        cert_der.to_str().unwrap(),
    ]);
    openssl(&[
        "pkcs8",
        "-topk8",
        "-nocrypt",
        "-in",
        key.to_str().unwrap(),
        "-outform",
        "DER",
        "-out",
        key_der.to_str().unwrap(),
    ]);
    (
        fs::read_to_string(ca).unwrap(),
        fs::read(cert_der).unwrap(),
        fs::read(key_der).unwrap(),
    )
}
