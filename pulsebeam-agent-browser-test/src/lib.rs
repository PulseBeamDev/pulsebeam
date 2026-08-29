#![cfg(test)]

use anyhow::{Context, Result, bail};
use thirtyfour::{By, DesiredCapabilities, WebDriver, prelude::ElementQueryable};

struct BrowserHarness {
    driver: WebDriver,
}

impl BrowserHarness {
    async fn connect() -> Result<Self> {
        let browser = std::env::var("BROWSER").unwrap_or_else(|_| String::from("chromium"));
        if browser == "safari" && !cfg!(target_os = "macos") {
            bail!("Safari/WebKit verification requires macOS with safaridriver enabled");
        }
        let endpoint = std::env::var("WEBDRIVER_URL")
            .context("WEBDRIVER_URL must point at chromedriver, geckodriver, or safaridriver")?;
        let driver = match browser.as_str() {
            "chromium" => WebDriver::new(&endpoint, DesiredCapabilities::chrome()).await?,
            "firefox" => WebDriver::new(&endpoint, DesiredCapabilities::firefox()).await?,
            "safari" => WebDriver::new(&endpoint, DesiredCapabilities::safari()).await?,
            _ => bail!("BROWSER must be chromium, firefox, or safari"),
        };
        Ok(Self { driver })
    }

    async fn open_harness(&self) -> Result<()> {
        let url = std::env::var("PULSEBEAM_AGENT_HARNESS_URL")
            .context("PULSEBEAM_AGENT_HARNESS_URL must identify the served WASM harness")?;
        self.driver.goto(url).await?;
        self.driver
            .query(By::Css("[data-agent-harness=ready]"))
            .first()
            .await?;
        let ready = self
            .driver
            .execute("return window.pulsebeamAgentHarnessReady", Vec::new())
            .await?
            .json()
            .as_bool()
            .unwrap_or(false);
        if !ready {
            bail!("WASM browser test harness did not report readiness");
        }
        Ok(())
    }

    async fn shutdown(self) -> Result<()> {
        self.driver.quit().await?;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires a real browser driver, a built WASM harness, and a local PulseBeam server"]
async fn browser_harness_loads() -> Result<()> {
    let harness = BrowserHarness::connect().await?;
    harness.open_harness().await?;
    harness.shutdown().await
}
