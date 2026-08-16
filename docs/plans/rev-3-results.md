# rev-3 execution results

## 100 us simulation baseline

The eight exact plan filters were run after changing only the simulator quantum.
All eight remained red, so none of the known failures was solely a 1 ms harness
artifact:

- `steady_state_allocation_does_not_churn_test`
- `priority_reconfiguration_quality_churn_test`
- `droppable_background_yields_to_focused_camera_test`
- `hidden_stream_frees_its_bandwidth_test`
- `subscriber_reaches_top_layer_on_a_rate_limited_link_test`
- `a_reordering_path_does_not_churn_keyframes_test`
- `still_screenshare_does_not_talk_down_a_healthy_link_test`
- `a_fixed_link_does_not_collapse_the_estimate`

The exact-filter run took 15.09 seconds at 100 us.
