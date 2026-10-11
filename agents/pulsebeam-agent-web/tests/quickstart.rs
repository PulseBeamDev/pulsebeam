use super::*;
use thirtyfour::By;

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

#[tokio::test(flavor = "multi_thread")]
async fn documented_react_quickstart_exchanges_media_and_cleans_up() -> TestResult<()> {
    let _destination = DestinationServer::start()?;
    let example = StaticServer::start(std::env::var("PULSEBEAM_REACT_QUICKSTART_ROOT")?).await?;
    let room = RoomExternalId::new("react-demo")?;
    let alice = mint_development_token(&room, &ParticipantExternalId::new("alice")?, u64::MAX)?;
    let bob = mint_development_token(&room, &ParticipantExternalId::new("bob")?, u64::MAX)?;
    let mut browser = capabilities()?;
    browser.add_arg("--window-size=1400,1000")?;
    browser.add_arg("--disable-dev-shm-usage")?;
    run_browser_test(managed_driver(browser.clone()), |first| async move {
        run_browser_test(managed_driver(browser), |second| async move {
            configure(&first, &example.url(""), &alice).await?;
            configure(&second, &example.url(""), &bob).await?;
            let first_bidi = first.bidi().await?;
            let first_context = first_bidi.browsing_context().top_level().await?;
            let second_bidi = second.bidi().await?;
            let second_context = second_bidi.browsing_context().top_level().await?;
            let _: bool = evaluate_json(&first_bidi, &first_context, "__quickstart.ready('alice', 'bob')").await?;
            let _: bool = evaluate_json(&second_bidi, &second_context, "__quickstart.ready('bob', 'alice')").await?;
            assert_eq!(example.wasm_requests(), 2, "both public bundles loaded the WASM asset");
            press(&first, "Leave and stop capture").await?;
            let _: bool = evaluate_json(&first_bidi, &first_context, "__quickstart.left()").await?;
            let _: bool = evaluate_json(&second_bidi, &second_context, "__quickstart.wait(() => !document.body.innerText.includes('alice: camera') && document.body.innerText.includes('Remote audio publications: 0'), 'remote withdrawal')").await?;
            press(&first, "Unmount room").await?;
            let _: bool = evaluate_json(&first_bidi, &first_context, "__quickstart.unmounted()").await?;
            press(&second, "Unmount room").await?;
            let _: bool = evaluate_json(&second_bidi, &second_context, "__quickstart.unmounted()").await?;
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        }).await?;
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    }).await?;
    Ok(())
}
