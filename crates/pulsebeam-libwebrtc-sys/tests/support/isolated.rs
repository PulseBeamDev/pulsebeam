use std::process::Command;

/// Process-wide invariants need a fresh test process, not just a serial test body.
pub fn run_in_child(name: &str) -> bool {
    const KEY: &str = "PULSEBEAM_ISOLATED_TEST";
    if std::env::var(KEY).as_deref() == Ok(name) {
        return false;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env(KEY, name)
        .output()
        .expect("start isolated test process");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "isolated test {name} failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    true
}
