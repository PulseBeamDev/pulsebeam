#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "live interoperability failures stop at the violated browser contract"
)]

mod browser_support;

use browser_support::{BrowserKind, TestResult, run_matrix};

#[test]
fn browser_harness_confines_classic_webdriver() {
    let harness = include_str!("browser_support/mod.rs");
    let scenarios = include_str!("browser_interop.rs");
    assert_eq!(harness.matches("WebDriver::new(").count(), 2);
    assert_eq!(harness.matches("driver.quit().await").count(), 1);
    for forbidden in [
        ".goto(",
        ".execute(",
        ".execute_async(",
        ".find(",
        ".get_log(",
        ".screenshot(",
        ".cdp(",
    ] {
        assert!(
            !harness.contains(forbidden),
            "classic interaction {forbidden}"
        );
    }
    let forbidden_crate = ["thirty", "four"].concat();
    assert!(!scenarios.contains(&forbidden_crate));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the repository-provisioned linux-x86_64 browser matrix"]
async fn chrome_matrix() -> TestResult<()> {
    run_matrix(BrowserKind::Chrome).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the repository-provisioned linux-x86_64 browser matrix"]
async fn firefox_matrix() -> TestResult<()> {
    run_matrix(BrowserKind::Firefox).await
}
