#[path = "support/destination.rs"]
mod destination;
mod support;
use destination::ExecutableServer;
use pulsebeam_core::identity::{ParticipantExternalId, RoomExternalId};
use std::error::Error;
use support::{
    StaticServer, TestResult, capabilities, evaluate_json, managed_driver, mint_development_token,
};
use thirtyfour::By;
use thirtyfour::{prelude::ChromiumLikeCapabilities, testing::run_browser_test};

async fn press(driver: &thirtyfour::WebDriver, label: &str) -> TestResult<()> {
    driver
        .find(By::XPath(format!("//button[text()='{label}']")))
        .await?
        .click()
        .await?;
    Ok(())
}

async fn configure(driver: &thirtyfour::WebDriver, url: &str, token: &str) -> TestResult<()> {
    driver.goto(url).await?;
    let bidi = driver.bidi().await?;
    let context = bidi.browsing_context().top_level().await?;
    let _: bool = evaluate_json(
        &bidi,
        &context,
        include_str!("contracts/react-quickstart-observer.js"),
    )
    .await?;
    driver
        .find(By::Css("input[name=token]"))
        .await?
        .send_keys(token)
        .await?;
    press(driver, "Open room").await?;
    press(driver, "Join").await?;
    press(driver, "Enable camera and microphone").await?;
    press(driver, "Enable remote audio").await?;
    Ok(())
}
