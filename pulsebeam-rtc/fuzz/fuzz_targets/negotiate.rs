#![no_main]

use libfuzzer_sys::fuzz_target;
use pulsebeam_rtc::{DtlsFingerprint, IceCandidate, IceCredentials, ServerTransport, negotiate};

fn server() -> ServerTransport {
    let ice = IceCredentials::new("localufrag".to_owned(), "localpassword".to_owned())
        .expect("valid local ICE credentials");
    let fingerprint = DtlsFingerprint::new("sha-256".to_owned(), Box::new([9; 32]))
        .expect("valid local fingerprint");
    let candidate =
        IceCandidate::new("candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned())
            .expect("valid candidate");
    ServerTransport::new(7, ice, fingerprint, Box::new([candidate]))
}

fuzz_target!(|data: &[u8]| {
    if let Ok(offer) = std::str::from_utf8(data) {
        let _ = negotiate(offer, &server());
    }
});
