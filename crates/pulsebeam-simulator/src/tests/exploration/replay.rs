use super::{Artifact, Execution};

fn execute(mut state: Execution, consume: bool) -> Execution {
    state.choose("declaration/1", vec!["publish".into(), "subscribe".into()]);
    state.choose("fault/1", vec!["none".into(), "loss".into()]);
    state.observe("positive fresh delivery and coherent owners".into());
    if consume {
        state.consume("declared fault at occurrence 1");
    }
    state.finish();
    state
}

fn recorded() -> Artifact {
    execute(Execution::new("build".into(), vec![], None), true).artifact
}

#[test]
fn replay_consumes_exact_choices_faults_and_semantic_observations() {
    let artifact = recorded();
    let replay = execute(
        Execution::new("build".into(), vec![], Some(artifact.clone())),
        true,
    );
    assert_eq!(replay.artifact, artifact);
    let unfired = execute(
        Execution::new("build".into(), vec![], Some(artifact)),
        false,
    );
    assert_eq!(unfired.artifact.outcome, "replay-divergence");
}

#[test]
fn changed_enabled_missing_extra_or_out_of_order_choices_cannot_replay() {
    let artifact = recorded();
    let mut incompatible = Vec::new();
    let mut changed = artifact.clone();
    changed.choices[0].enabled.reverse();
    incompatible.push(changed);
    let mut missing = artifact.clone();
    missing.choices.pop();
    incompatible.push(missing);
    let mut extra = artifact.clone();
    extra.choices.push(extra.choices[0].clone());
    incompatible.push(extra);
    let mut reordered = artifact.clone();
    reordered.choices.reverse();
    incompatible.push(reordered);
    let mut invalid_selection = artifact;
    invalid_selection.choices[0].selected = 10;
    incompatible.push(invalid_selection);
    for recorded in incompatible {
        let state = execute(Execution::new("build".into(), vec![], Some(recorded)), true);
        assert_eq!(state.artifact.outcome, "replay-divergence");
    }
}

#[test]
fn changed_bounds_identity_and_properties_are_rejected() {
    let artifact = recorded();
    let mut changed = artifact.clone();
    changed.scenario.horizon_ms += 1;
    assert_eq!(
        Execution::new("build".into(), vec![], Some(changed))
            .artifact
            .outcome,
        "replay-divergence"
    );
    assert_eq!(
        Execution::new("other-build".into(), vec![], Some(artifact.clone()))
            .artifact
            .outcome,
        "replay-divergence"
    );
    let mut changed = artifact.clone();
    changed.observations[0] = "missing post-recovery delivery".into();
    assert_eq!(
        execute(Execution::new("build".into(), vec![], Some(changed)), true)
            .artifact
            .outcome,
        "replay-divergence"
    );
    let mut changed = execute(Execution::new("build".into(), vec![], None), true);
    changed.replay = Some(artifact);
    changed.reject("counterexample", "new semantic failure".into());
    changed.finish();
    assert_eq!(changed.artifact.outcome, "replay-divergence");
}

#[test]
fn reproduced_counterexample_remains_a_counterexample_not_a_pass() {
    let mut original = execute(Execution::new("build".into(), vec![], None), true);
    original.reject("counterexample", "fresh delivery missing".into());
    let artifact = original.artifact.clone();
    let mut replay = Execution::new("build".into(), vec![], Some(artifact.clone()));
    replay.choose("declaration/1", vec!["publish".into(), "subscribe".into()]);
    replay.choose("fault/1", vec!["none".into(), "loss".into()]);
    replay.observe("positive fresh delivery and coherent owners".into());
    replay.consume("declared fault at occurrence 1");
    replay.reject("counterexample", "fresh delivery missing".into());
    replay.finish();
    assert_eq!(replay.artifact, artifact);
    assert_eq!(replay.artifact.outcome, "counterexample");
}
