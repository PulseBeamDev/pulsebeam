use proptest::test_runner::FileFailurePersistence;
use std::path::PathBuf;
use std::sync::OnceLock;

static ARTIFACT: OnceLock<PathBuf> = OnceLock::new();

pub(super) fn persistence() -> FileFailurePersistence {
    FileFailurePersistence::Direct(artifact().to_str().expect("UTF-8 regression artifact path"))
}

fn artifact() -> &'static std::path::Path {
    ARTIFACT.get_or_init(|| {
        let family = std::thread::current().name().unwrap_or("properties").replace("::", "_");
        let root = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR").map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(format!("pulsebeam-sim-regressions-{}", std::process::id())));
        std::fs::create_dir_all(&root).expect("writable regression artifact directory");
        let path = root.join(format!("{family}.regressions"));
        let corpus = match std::env::var_os("PULSEBEAM_SIM_REGRESSIONS") {
            Some(input) => std::fs::read(input).expect("read requested regression corpus"),
            None => include_bytes!("../properties.regressions").to_vec(),
        };
        // Persisted seeds are RNG states, not serialized scenarios. Historical shrink comments
        // describe the former generator; every seed is evaluated by the current family's strategy.
        let mut contents = format!("# Current strategy family: {family}\n# Imported seed comments are historical, not current scenario fixtures.\n").into_bytes();
        contents.extend_from_slice(&corpus);
        std::fs::write(&path, contents).expect("initialize writable regression corpus");
        eprintln!("[simulation-regressions] family={family} artifact={}", path.display());
        path
    })
}

#[test]
fn committed_and_minimized_failures_replay_in_fresh_processes() {
    const PROBE: &str = "tests::properties::persistence::regression_probe";
    let root = std::env::var_os("TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("pulsebeam-regression-proof-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let empty = root.join("empty.regressions");
    std::fs::write(&empty, "# initially empty\n").unwrap();
    let run = |mode: &str, input: Option<&std::path::Path>, output: &str| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                PROBE,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("PULSEBEAM_REGRESSION_PROBE", mode)
            .env_remove("PULSEBEAM_SIM_REGRESSIONS")
            .env("TEST_UNDECLARED_OUTPUTS_DIR", root.join(output))
            .env("RUST_LOG", "off");
        if let Some(input) = input {
            command.env("PULSEBEAM_SIM_REGRESSIONS", input);
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout)
            .unwrap()
            .lines()
            .find(|line| line.starts_with("[regression-probe]"))
            .expect("probe result")
            .to_owned()
    };
    let recorded = run("record", Some(&empty), "recorded");
    let saved = root
        .join("recorded")
        .join(format!("{}.regressions", PROBE.replace("::", "_")));
    assert!(
        std::fs::read_to_string(&saved)
            .unwrap()
            .lines()
            .any(|line| line.starts_with("cc ")),
        "failed RNG state was not persisted"
    );
    let replayed = run("replay", Some(&saved), "replayed");
    assert_eq!(
        recorded, replayed,
        "a fresh process with a different RNG and zero novel cases must replay the saved failure and shrink to the same input"
    );
    let committed = run("committed", None, "committed");
    let seeds = include_str!("../properties.regressions")
        .lines()
        .filter(|line| line.starts_with("cc "))
        .count();
    assert!(seeds > 0, "committed replay must exercise retained inputs");
    assert_eq!(
        committed,
        format!("[regression-probe] committed={seeds}"),
        "zero novel cases must replay every committed RNG state under the current ample-family strategy"
    );
}

#[test]
#[ignore = "subprocess fixture for regression persistence assertions"]
fn regression_probe() {
    use proptest::test_runner::{
        Config, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner,
    };
    let Ok(mode) = std::env::var("PULSEBEAM_REGRESSION_PROBE") else {
        return;
    };
    assert!(matches!(mode.as_str(), "record" | "replay" | "committed"));
    let replay = mode != "record";
    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: if replay { 0 } else { 1 },
            max_shrink_iters: 128,
            failure_persistence: Some(Box::new(persistence())),
            ..Config::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &[if replay { 91 } else { 7 }; 32]),
    );
    if mode == "committed" {
        let visited = std::cell::Cell::new(0);
        runner
            .run(
                &super::scenarios(super::Demand::any(), super::Budget::Ample, super::ANY_FAULT),
                |scenario| {
                    visited.set(visited.get() + 1);
                    assert!(
                        super::Budget::Ample.admits(scenario.capacity_bps, scenario.demand.bps())
                    );
                    Ok(())
                },
            )
            .expect("committed seeds replay under the current strategy");
        println!("\n[regression-probe] committed={}", visited.get());
        return;
    }
    let first = std::cell::Cell::new(None);
    let result = runner.run(&(1..1000u32), |value| {
        if first.get().is_none() {
            first.set(Some(value));
        }
        Err(TestCaseError::fail("deliberate persistence probe"))
    });
    let Err(TestError::Fail(_, minimum)) = result else {
        panic!("the deliberately failing property was not replayed: {result:?}");
    };
    assert_eq!(minimum, 1, "the failure was not minimized");
    println!(
        "\n[regression-probe] first={} minimum={minimum}",
        first.get().expect("property executed")
    );
}
