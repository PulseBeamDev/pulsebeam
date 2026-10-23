use super::alloc_test_support::{state_of_mut, video_track_with_states};
use super::*;
use crate::entity::ParticipantId;
use crate::media::SimulcastLayer;

fn camera() -> (Track, LayerStates) {
    let (tx, built, mut states) = video_track_with_states(
        ParticipantId::new(),
        Mid::from("v0"),
        ["q", "h", "f"].map(SimulcastLayer::new).to_vec(),
    );
    let track = Track::video(tx.meta, built.layers().to_vec(), None);
    for (quality, bitrate) in [
        (LayerQuality::Low, 150_000),
        (LayerQuality::Medium, 400_000),
        (LayerQuality::High, 1_250_000),
    ] {
        state_of_mut(&mut states, track.by_quality(quality).unwrap())
            .update_for_test()
            .bitrate(bitrate);
    }
    (track, states)
}

fn scalable_camera() -> (Track, LayerStates) {
    let (tx, built, mut states) = video_track_with_states(
        ParticipantId::new(),
        Mid::from("v0"),
        vec![SimulcastLayer::new("f")],
    );
    let track = Track::video(tx.meta, built.layers().to_vec(), None);
    let layer = track.by_quality(LayerQuality::High).unwrap();
    let state = state_of_mut(&mut states, layer);
    state.update_for_test().bitrate(300_000);
    state.set_decode_target_count(3);
    state.set_temporal_ladder(&[50, 150, 300], 30);
    state.decode_target_heights = [720; 3];
    (track, states)
}

fn view<'a>(track: &'a Track, key: DownstreamSlotKey, mid: &str, priority: u32) -> SlotView<'a> {
    SlotView {
        key,
        mid: Mid::from(mid),
        max_height: 720,
        min_height: 180,
        min_fps: 0,
        priority,
        track,
        current_quality: LayerQuality::High,
        current_decode_target: DecodeTargetSelection::Full,
        forwarding: true,
    }
}

fn forwarded_quality(decision: AllocationDecision<'_>) -> Option<LayerQuality> {
    match decision {
        AllocationDecision::Forward(layer, _) | AllocationDecision::ForwardTarget(layer, _, _) => {
            Some(layer.quality)
        }
        AllocationDecision::Pause(_, _) => None,
    }
}

#[test]
fn positive_target_enforces_one_aggregate_minimum_but_zero_still_suspends() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let first = keys.insert(());
    let second = keys.insert(());
    let views = [
        SlotView {
            forwarding: false,
            ..view(&track, first, "a", 200)
        },
        SlotView {
            forwarding: false,
            ..view(&track, second, "b", 100)
        },
    ];
    let engine = AllocationEngine::new(&views, &states);
    let mut gates = views
        .iter()
        .map(|slot| (slot.key, ResumeGate { required: true }))
        .collect();
    let decisions = engine.run_compute_with_resume(Bitrate::from(1_000), &views, &mut gates);
    assert_eq!(forwarded_quality(decisions[first]), Some(LayerQuality::Low));
    assert_eq!(forwarded_quality(decisions[second]), None);
    assert_eq!(
        AllocationEngine::used_bitrate(&decisions),
        Bitrate::from(150_000)
    );

    let suspended = engine.run_compute_with_resume(Bitrate::ZERO, &views, &mut gates);
    assert_eq!(forwarded_quality(suspended[first]), None);
    assert_eq!(forwarded_quality(suspended[second]), None);
    assert_eq!(AllocationEngine::used_bitrate(&suspended), Bitrate::ZERO);
}

#[test]
fn lower_priority_minimum_outranks_higher_priority_upgrade() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let high = keys.insert(());
    let low = keys.insert(());
    let views = [
        view(&track, high, "a", 200),
        SlotView {
            min_height: 720,
            ..view(&track, low, "b", 10)
        },
    ];
    let engine = AllocationEngine::new(&views, &states);
    let decisions = engine.run_compute(Bitrate::from(1_600_000), &views);
    assert_eq!(forwarded_quality(decisions[high]), Some(LayerQuality::Low));
    assert_eq!(forwarded_quality(decisions[low]), Some(LayerQuality::High));
    assert_eq!(
        AllocationEngine::used_bitrate(&decisions),
        Bitrate::from(1_400_000)
    );
}

#[test]
fn gallery_upgrades_only_after_all_minimums_and_headroom_fit() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let views: Vec<_> = ["a", "b", "c", "d", "e", "f"]
        .into_iter()
        .enumerate()
        .map(|(index, mid)| SlotView {
            max_height: if index == 0 { 720 } else { 180 },
            current_quality: if index == 0 {
                LayerQuality::Medium
            } else {
                LayerQuality::Low
            },
            ..view(
                &track,
                keys.insert(()),
                mid,
                if index == 0 { 100 } else { 10 },
            )
        })
        .collect();
    let engine = AllocationEngine::new(&views, &states);
    for (budget, speaker) in [
        (2_045_000, LayerQuality::Medium),
        (3_000_000, LayerQuality::High),
    ] {
        let decisions = engine.run_compute(Bitrate::from(budget), &views);
        for (index, slot) in views.iter().enumerate() {
            assert_eq!(
                forwarded_quality(decisions[slot.key]),
                Some(if index == 0 {
                    speaker
                } else {
                    LayerQuality::Low
                })
            );
        }
        assert!(AllocationEngine::used_bitrate(&decisions) <= Bitrate::from(budget));
    }
}

#[test]
fn higher_priority_minimum_wins_scarcity_and_lower_priority_degrades_or_pauses() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let high = keys.insert(());
    let low = keys.insert(());
    let views = [
        SlotView {
            min_height: 720,
            ..view(&track, high, "a", 200)
        },
        SlotView {
            min_height: 720,
            ..view(&track, low, "b", 10)
        },
    ];
    let engine = AllocationEngine::new(&views, &states);
    let degraded = engine.run_compute(Bitrate::from(1_450_000), &views);
    assert_eq!(forwarded_quality(degraded[high]), Some(LayerQuality::High));
    assert_eq!(forwarded_quality(degraded[low]), Some(LayerQuality::Low));
    let paused = engine.run_compute(Bitrate::from(1_250_000), &views);
    assert_eq!(forwarded_quality(paused[high]), Some(LayerQuality::High));
    assert_eq!(forwarded_quality(paused[low]), None);
}

#[test]
fn zero_minimums_do_not_reserve_bandwidth_against_priority_targets() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let high = keys.insert(());
    let low = keys.insert(());
    let views = [
        view(&track, high, "a", 200),
        SlotView {
            min_height: 0,
            ..view(&track, low, "b", 10)
        },
    ];
    let engine = AllocationEngine::new(&views, &states);
    let decisions = engine.run_compute(Bitrate::from(1_450_000), &views);
    assert_eq!(forwarded_quality(decisions[high]), Some(LayerQuality::High));
    assert_eq!(forwarded_quality(decisions[low]), Some(LayerQuality::Low));
    let decisions = engine.run_compute(Bitrate::from(1_300_000), &views);
    assert_eq!(forwarded_quality(decisions[high]), Some(LayerQuality::High));
    assert_eq!(forwarded_quality(decisions[low]), None);
}

#[test]
fn temporal_only_minimums_are_reserved_before_full_frame_rate_upgrades() {
    let (track, states) = scalable_camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let high = keys.insert(());
    let low = keys.insert(());
    let views = [
        SlotView {
            min_height: 0,
            min_fps: 15,
            ..view(&track, high, "a", 200)
        },
        SlotView {
            min_height: 0,
            min_fps: 15,
            ..view(&track, low, "b", 10)
        },
    ];
    let engine = AllocationEngine::new(&views, &states);
    let decisions = engine.run_compute(Bitrate::from(400_000), &views);
    for key in [high, low] {
        assert!(matches!(
            decisions[key],
            AllocationDecision::ForwardTarget(_, DecodeTargetSelection::Target(1), _)
        ));
    }
    assert_eq!(
        AllocationEngine::used_bitrate(&decisions),
        Bitrate::from(300_000)
    );
}

#[test]
fn unknown_fps_protection_prefers_full_temporal_service_when_affordable() {
    let (track, mut states) = scalable_camera();
    let layer = track.by_quality(LayerQuality::High).unwrap();
    state_of_mut(&mut states, layer).full_fps = 0;
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let key = keys.insert(());
    let views = [SlotView {
        min_height: 0,
        min_fps: 15,
        ..view(&track, key, "a", 10)
    }];
    let engine = AllocationEngine::new(&views, &states);
    let full = engine.run_compute(Bitrate::from(300_000), &views);
    assert!(matches!(full[key], AllocationDecision::Forward(_, _)));
    let partial = engine.run_compute(Bitrate::from(200_000), &views);
    assert!(matches!(
        partial[key],
        AllocationDecision::ForwardTarget(_, DecodeTargetSelection::Target(1), _)
    ));
}

#[test]
fn frame_rate_only_demand_keeps_full_rate_with_unknown_spatial_targets() {
    let (track, mut states) = scalable_camera();
    let layer = track.by_quality(LayerQuality::High).unwrap();
    state_of_mut(&mut states, layer).decode_target_heights = [0; 3];
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let key = keys.insert(());
    let views = [SlotView {
        max_height: 0,
        min_height: 0,
        min_fps: 15,
        ..view(&track, key, "a", 10)
    }];
    let engine = AllocationEngine::new(&views, &states);
    let decisions = engine.run_compute(Bitrate::from(1_000_000), &views);
    assert!(matches!(decisions[key], AllocationDecision::Forward(_, _)));
    assert!(engine.run_desired(&views) >= Bitrate::from(300_000));
}

#[test]
fn fps_minimum_can_raise_the_spatial_target_to_an_available_encoding() {
    let (track, mut states) = camera();
    for layer in track.layers() {
        state_of_mut(&mut states, layer).full_fps = if layer.quality == LayerQuality::Low {
            15
        } else {
            30
        };
    }
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let key = keys.insert(());
    let views = [SlotView {
        max_height: 180,
        min_height: 0,
        min_fps: 30,
        ..view(&track, key, "a", 10)
    }];
    let engine = AllocationEngine::new(&views, &states);
    let decisions = engine.run_compute(Bitrate::from(500_000), &views);
    assert_eq!(
        forwarded_quality(decisions[key]),
        Some(LayerQuality::Medium)
    );
}

#[test]
fn unavailable_fps_and_height_minimums_get_the_best_available_source() {
    let (track, mut states) = camera();
    for layer in track.layers() {
        state_of_mut(&mut states, layer).full_fps = 30;
    }
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let key = keys.insert(());
    let views = [SlotView {
        min_height: 1080,
        min_fps: 60,
        ..view(&track, key, "a", 10)
    }];
    let engine = AllocationEngine::new(&views, &states);
    let decisions = engine.run_compute(Bitrate::from(2_000_000), &views);
    assert_eq!(forwarded_quality(decisions[key]), Some(LayerQuality::High));
    let decisions = engine.run_compute(Bitrate::from(500_000), &views);
    assert_eq!(
        forwarded_quality(decisions[key]),
        Some(LayerQuality::Medium)
    );
}

#[test]
fn spatial_decode_target_minimum_is_joint_with_the_temporal_minimum() {
    let (track, mut states) = scalable_camera();
    let layer = track.by_quality(LayerQuality::High).unwrap();
    state_of_mut(&mut states, layer).decode_target_heights = [180, 360, 720];
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let key = keys.insert(());
    let views = [SlotView {
        min_height: 360,
        min_fps: 30,
        ..view(&track, key, "a", 10)
    }];
    let engine = AllocationEngine::new(&views, &states);
    let choices = engine.choices(&views[0]);
    let minimum =
        AllocationEngine::minimum_choice(&choices, &views[0], 300_000.0, ResumeGate::default())
            .unwrap();
    assert_eq!(minimum.target, DecodeTargetSelection::Target(1));
    assert_eq!(minimum.minimum_rank(&views[0]), (true, 360, 30));
    let decisions = engine.run_compute(Bitrate::from(200_000), &views);
    assert!(matches!(
        decisions[key],
        AllocationDecision::ForwardTarget(_, DecodeTargetSelection::Target(1), _)
    ));
}

#[test]
fn protected_minimums_fit_for_every_feasible_budget_without_overspend() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let views: Vec<_> = [180, 360, 720, 180, 360]
        .into_iter()
        .enumerate()
        .map(|(i, floor)| SlotView {
            min_height: floor,
            ..view(
                &track,
                keys.insert(()),
                &format!("v{i}"),
                100 - u32::try_from(i).unwrap(),
            )
        })
        .collect();
    let engine = AllocationEngine::new(&views, &states);
    let minimum_cost = 150_000 + 400_000 + 1_250_000 + 150_000 + 400_000;
    for budget in (100_000..8_000_000).step_by(50_000) {
        let decisions = engine.run_compute(Bitrate::from(budget), &views);
        let used = AllocationEngine::used_bitrate(&decisions);
        if budget < 150_000 {
            assert_eq!(used, Bitrate::from(150_000));
            assert_eq!(
                decisions
                    .values()
                    .filter(|decision| forwarded_quality(**decision).is_some())
                    .count(),
                1,
            );
        } else {
            assert!(used <= Bitrate::from(budget));
        }
        if budget >= minimum_cost {
            for view in &views {
                match decisions[view.key] {
                    AllocationDecision::Forward(layer, _) => {
                        assert!(engine.height(layer) >= view.min_height);
                    }
                    _ => panic!("every available minimum fits at {budget}"),
                }
            }
        }
    }
}

#[test]
fn upgrades_need_headroom_and_downgrades_do_not_oscillate_at_the_boundary() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let key = keys.insert(());
    for (current, budget, expected) in [
        (LayerQuality::High, 1_260_000, LayerQuality::High),
        (LayerQuality::High, 1_240_000, LayerQuality::Medium),
        (LayerQuality::Medium, 1_260_000, LayerQuality::Medium),
        (LayerQuality::Medium, 1_400_000, LayerQuality::High),
    ] {
        let views = [SlotView {
            current_quality: current,
            ..view(&track, key, "a", 10)
        }];
        let engine = AllocationEngine::new(&views, &states);
        let decisions = engine.run_compute(Bitrate::from(budget), &views);
        assert_eq!(forwarded_quality(decisions[key]), Some(expected));
    }
}

fn equal_priority_congestion_serves_every_peer(min_height: u32) {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let mut views: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|mid| SlotView {
            max_height: 180,
            ..view(&track, keys.insert(()), mid, 10)
        })
        .collect();
    for view in &mut views {
        view.min_height = min_height;
    }
    let mut served = HashSet::new();
    let mut gates: SecondaryMap<_, _> = views
        .iter()
        .map(|v| (v.key, ResumeGate::default()))
        .collect();
    let mut fairness = AllocationFairness::default();
    let start = Instant::now();
    for tick in 0..280 {
        let now = start + Duration::from_millis(tick * 100);
        fairness.order(&mut views, now);
        let engine = AllocationEngine::new(&views, &states);
        let decisions = engine.run_compute_with_resume(Bitrate::from(200_000), &views, &mut gates);
        fairness.observe(&views, &engine, &decisions, Bitrate::from(200_000), now);
        let outcomes: Vec<_> = decisions
            .iter()
            .map(|(key, decision)| (key, forwarded_quality(*decision)))
            .collect();
        drop(decisions);
        for v in &mut views {
            let quality = outcomes.iter().find(|(key, _)| *key == v.key).unwrap().1;
            if let Some(quality) = quality {
                served.insert(v.mid);
                v.current_quality = quality;
                gates.insert(v.key, ResumeGate::default());
            } else if v.forwarding {
                gates.insert(v.key, ResumeGate { required: true });
            }
            v.forwarding = quality.is_some();
        }
    }
    assert_eq!(
        served.len(),
        3,
        "MID order must not permanently starve any peer"
    );
}

#[test]
fn equal_priority_protected_peers_survive_mid_order_and_resume_headroom() {
    equal_priority_congestion_serves_every_peer(180);
}

#[test]
fn equal_priority_optional_peers_survive_mid_order_and_resume_headroom() {
    equal_priority_congestion_serves_every_peer(0);
}

#[test]
fn equal_priority_peers_take_turns_meeting_minimums_not_just_avoiding_pause() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let mut views: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|mid| SlotView {
            min_height: 720,
            ..view(&track, keys.insert(()), mid, 10)
        })
        .collect();
    let mut fairness = AllocationFairness::default();
    let mut served_minimum = HashSet::new();
    let start = Instant::now();
    for second in 0..12 {
        let now = start + Duration::from_secs(second);
        fairness.order(&mut views, now);
        let engine = AllocationEngine::new(&views, &states);
        let decisions = engine.run_compute(Bitrate::from(1_450_000), &views);
        fairness.observe(&views, &engine, &decisions, Bitrate::from(1_450_000), now);
        for slot in &views {
            if forwarded_quality(decisions[slot.key]) == Some(LayerQuality::High) {
                served_minimum.insert(slot.mid);
            }
            assert!(forwarded_quality(decisions[slot.key]).is_some());
        }
    }
    assert_eq!(served_minimum.len(), 2);
}

#[test]
fn an_optional_pause_does_not_rotate_satisfied_protected_peers() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let mut views = [
        view(&track, keys.insert(()), "a", 10),
        view(&track, keys.insert(()), "b", 10),
        SlotView {
            min_height: 0,
            ..view(&track, keys.insert(()), "c", 10)
        },
    ];
    let mut fairness = AllocationFairness::default();
    let start = Instant::now();
    for second in 0..20 {
        let now = start + Duration::from_secs(second);
        fairness.order(&mut views, now);
        let engine = AllocationEngine::new(&views, &states);
        let decisions = engine.run_compute(Bitrate::from(350_000), &views);
        fairness.observe(&views, &engine, &decisions, Bitrate::from(350_000), now);
        assert_eq!(views[0].mid, Mid::from("a"));
        assert_eq!(views[1].mid, Mid::from("b"));
        assert_eq!(forwarded_quality(decisions[views[2].key]), None);
    }
}

#[test]
fn affordable_equal_priority_minimums_keep_a_stable_order() {
    let (track, states) = camera();
    let mut keys = SlotMap::<DownstreamSlotKey, ()>::with_key();
    let mut views: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|mid| SlotView {
            max_height: 180,
            ..view(&track, keys.insert(()), mid, 10)
        })
        .collect();
    let mut fairness = AllocationFairness::default();
    let start = Instant::now();
    for second in 0..60 {
        let now = start + Duration::from_secs(second);
        fairness.order(&mut views, now);
        let engine = AllocationEngine::new(&views, &states);
        let decisions = engine.run_compute_with_resume(
            Bitrate::from(500_000),
            &views,
            &mut SecondaryMap::new(),
        );
        fairness.observe(&views, &engine, &decisions, Bitrate::from(500_000), now);
        assert_eq!(
            views.iter().map(|v| v.mid).collect::<Vec<_>>(),
            [Mid::from("a"), Mid::from("b"), Mid::from("c")]
        );
        assert!(
            decisions
                .values()
                .all(|decision| forwarded_quality(*decision).is_some())
        );
    }
}
