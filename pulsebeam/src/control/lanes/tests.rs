//! What a lane registry still owns: which arena a key belongs to.
//!
//! Who publishes what, and which streams a topic has, moved to
//! `control::publication` when both legs were filed in one catalog. What is
//! left is the mapping from lane to arena, which is the only thing about a
//! data lane the routing layer needs.

use super::*;

type Registry = LaneRegistry;

#[test]
fn each_lane_mints_and_actions_only_its_own_key() {
    let data = Registry::new(StreamLane::Unreliable);
    let reliable = Registry::new(StreamLane::Reliable);
    let data_key = RuntimeStreamKey::Unreliable(Default::default());
    let reliable_key = RuntimeStreamKey::Reliable(Default::default());

    assert!(matches!(
        data.route_action(data_key),
        Some(RouteAction::Unreliable { .. })
    ));
    assert!(matches!(
        reliable.route_action(reliable_key),
        Some(RouteAction::Reliable { .. })
    ));
    assert!(
        data.route_action(reliable_key).is_none(),
        "a lane must refuse the other lane's key rather than mislabel it"
    );
    assert!(reliable.route_action(data_key).is_none());
}
