use super::*;

type Registry = LaneRegistry;

#[test]
fn every_stream_lane_uses_the_same_route_key_shape() {
    let data = Registry::new(StreamLane::Unreliable);
    let reliable = Registry::new(StreamLane::Reliable);
    let data_key: crate::keys::TrackKey = Default::default();
    let reliable_key: crate::keys::TrackKey = Default::default();

    assert!(matches!(data.route_action(data_key), RouteAction::Forward { .. }));
    assert!(matches!(
        reliable.route_action(reliable_key),
        RouteAction::Forward { .. }
    ));
}
