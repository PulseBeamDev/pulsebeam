mod support;

use support::PeerFixture;

#[test]
fn native_l1t3_source_has_authenticated_dependency_context() {
    let mut fixture = PeerFixture::connected_video_dd();
    let source = fixture.send_video_temporal_source(b"\x65native DD", Some(0));
    assert!(source.extension(pulsebeam_core::dd::URI).is_some());
    assert_eq!(
        source
            .dependency_descriptor_context()
            .expect("authenticated DD context")
            .1,
        15
    );
}
