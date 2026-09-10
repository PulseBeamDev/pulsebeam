#[path = "../src/congestion.rs"]
mod congestion;

#[test]
fn fixed_scenario_matrix() {
    for metrics in congestion::scenario::run_fixed_scenario_matrix() {
        eprintln!(
            "case={} seed={:#06x} metrics={metrics:?} excluded=first-five-rtts,path-steps,probe-clusters,keyframe",
            metrics.name, metrics.seed
        );
        assert!(metrics.maximum_native_target_micros <= 400_000);
        assert!(metrics.probe_overhead_percent <= 5);
        assert_eq!(metrics.duplicate_status_consumptions, 0);
        match metrics.seed {
            0x0701 => {
                assert!(metrics.utilization_percent >= 85);
                assert!(metrics.p99_queue_micros <= metrics.effective_target_micros + 10_000);
            }
            0x0702 => {
                assert_eq!(metrics.application_limited_at_millis, Some(200));
                assert!(metrics.admitted_percent >= 95);
            }
            0x0703 => {
                assert!(metrics.stale_at_millis.is_some());
                assert!(!metrics.window_grew_while_stale);
                assert!(metrics.stale_recovered);
            }
            0x0704 => {
                assert!(metrics.p99_queue_micros <= metrics.effective_target_micros + 10_000);
            }
            0x0705 => {
                assert_eq!(metrics.baseline_resets, 1);
                assert!(!metrics.old_path_sample_used);
            }
            0x0706 => {
                assert!(metrics.ecn_reduced_window);
                assert!(metrics.l4s_disabled_on_bleach);
            }
            0x0707 => assert!(metrics.policer_detected),
            0x0708 => {
                assert!((40..=60).contains(&metrics.fairness_percent));
                assert!(metrics.utilization_percent >= 85);
            }
            _ => panic!("unreviewed scenario seed"),
        }
    }
}
