use super::*;
use thirtyfour::By;

async fn click(driver: &thirtyfour::WebDriver, label: &str) -> TestResult<()> {
    driver
        .find(By::Css(format!("[aria-label='{label}']")))
        .await?
        .click()
        .await?;
    Ok(())
}

fn observed_intents(
    messages: Vec<Vec<u8>>,
) -> TestResult<Vec<pulsebeam_proto::signaling_v1::Intent>> {
    let mut intents = Vec::new();
    for bytes in messages {
        let message = pulsebeam_proto::codec::decode_client(&bytes)
            .map_err(|error| format!("invalid observed signaling: {error:?}"))?;
        if let Some(pulsebeam_proto::signaling_v1::client_message::Payload::Intent(intent)) =
            message.payload
        {
            intents.push(intent);
        }
    }
    Ok(intents)
}

#[tokio::test(flavor = "multi_thread")]
async fn meet_topics_and_recovery_preserve_real_media() -> TestResult<()> {
    meet_media_contract(false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn meet_gesture_before_media_preserves_real_media() -> TestResult<()> {
    meet_media_contract(true).await
}

async fn meet_media_contract(gesture_before_media: bool) -> TestResult<()> {
    let destination = DestinationServer::start()?;
    let meet = StaticServer::start(std::env::var("PULSEBEAM_MEET_ROOT")?).await?;
    let sdk = StaticServer::start(root()).await?;
    let room = RoomExternalId::new("meet-media-continuity")?;
    let receiver = mint_development_token(
        &room,
        &ParticipantExternalId::new("meet-receiver")?,
        u64::MAX,
    )?;
    let sender =
        mint_development_token(&room, &ParticipantExternalId::new("meet-sender")?, u64::MAX)?;
    let publisher = include_str!("contracts/meet-publisher-start.js")
        .replace("__SDK_URL__", &sdk.url("dist/index.js"))
        .replace("__SENDER_TOKEN__", &sender);
    let join = format!(
        r#"(() => {{
        const fields = document.querySelectorAll('form input');
        const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
        for (const [index, value] of [{receiver:?}, 'http://127.0.0.1:7070'].entries()) {{
            set.call(fields[index], value);
            fields[index].dispatchEvent(new Event('input', {{ bubbles: true }}));
        }}
        return true;
    }})()"#
    );
    let mut browser = capabilities()?;
    browser.add_arg("--autoplay-policy=document-user-activation-required")?;
    browser.add_arg("--window-size=1400,1000")?;
    browser.add_arg("--disable-dev-shm-usage")?;
    let browser_log = std::path::PathBuf::from(std::env::var("TEST_UNDECLARED_OUTPUTS_DIR")?)
        .join("meet-chrome.log");
    browser.add_arg("--enable-logging")?;
    browser.add_arg(&format!("--log-file={}", browser_log.display()))?;
    let result = run_browser_test(managed_driver(browser), |driver| async move {
        eprintln!("Meet stage: navigation");
        driver.goto(meet.url("")).await?;
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        let _: bool = evaluate_json(&bidi, &context, include_str!("contracts/meet-media-observer.js")).await?;
        eprintln!("Meet stage: lobby join");
        let _: bool = evaluate_json(&bidi, &context, r#"(async () => {
            await __meet.wait(() => document.querySelector('button[type=submit]')?.disabled === false, 'hydrated lobby capture');
            return true;
        })()"#).await?;
        let _: bool = evaluate_json(&bidi, &context, &join).await?;
        let _: bool = evaluate_json(&bidi, &context, r#"(async () => {
            document.querySelector('form').requestSubmit();
            await __meet.wait(() => document.querySelector('[aria-label=Reconnect]'), 'real Room mounted');
            return true;
        })()"#).await?;
        if gesture_before_media {
            eprintln!("Meet stage: gesture before publisher");
            let _: bool = evaluate_json(&bidi, &context, r#"(() => {
                __meet.assert(__meet.audioNodes().length === 0, 'gesture precedes remote audio');
                return true;
            })()"#).await?;
            click(&driver, "Chat").await?;
        }
        eprintln!("Meet stage: publisher");
        let _: bool = evaluate_json(&bidi, &context, &publisher).await?;
        if !gesture_before_media {
            let _: bool = evaluate_json(&bidi, &context, r#"(async () => {
                await __meet.wait(() => __meet.audioNodes().length === 1, 'audio route arrives before any trusted gesture');
                await __meet.wait(() => __meet.audioNodes()[0].context.state === 'suspended' && __meet.blockedResumes() > 0, 'blocked audio policy before ordinary Meet gesture');
                __meet.assert(!/Enable audio|Retry playback|Unlock audio/i.test(document.body.innerText), 'no unlock prompt');
                return true;
            })()"#).await?;
            eprintln!("Meet stage: audio gesture");
            click(&driver, "Chat").await?;
        }
        let _: bool = evaluate_json(&bidi, &context, "__meet.ready()").await?;
        let baseline: Vec<Vec<u8>> = evaluate_json(&bidi, &context, "__meet.mark()").await?;
        let baseline = observed_intents(baseline)?.pop().ok_or("missing baseline receive intent")?;
        let desired = baseline.receive.and_then(|receive| receive.video).ok_or("missing baseline video intent")?;
        assert!(!desired.tracks.is_empty(), "baseline has real video demand");
        driver.find(By::Css("form input")).await?.send_keys("outbound Meet chat").await?;
        click(&driver, "Send message").await?;
        click(&driver, "Send a reaction").await?;
        click(&driver, "React with 👍").await?;
        let _: bool = evaluate_json(&bidi, &context, r#"(async () => {
            await __meet.wait(() => __meet.chats.some(({text}) => text === 'outbound Meet chat') && __meet.emojis.some(({emoji}) => emoji === '👍'), 'real Meet sends both topics');
            await __meet.chat.publish({sender:'meet-sender', text:'inbound Meet chat', id:crypto.randomUUID(), ts:Date.now()});
            await __meet.reactions.publish({sender:'meet-sender', emoji:'🔥', id:crypto.randomUUID(), ts:Date.now()});
            await __meet.wait(() => document.body.innerText.includes('inbound Meet chat') && document.querySelector('.meet-reaction[aria-label="🔥"]'), 'real Meet receives and renders both topics');
            return true;
        })()"#).await?;
        eprintln!("Meet stage: topic continuity");
        let intents: Vec<Vec<u8>> = evaluate_json(&bidi, &context, "__meet.continuity()").await?;
        for intent in observed_intents(intents)? {
            let receive = intent.receive.ok_or("missing receive intent")?;
            assert_eq!(receive.audio.map_or(0, |audio| audio.mode), 0, "UI traffic retains automatic audio selection");
            let video = receive.video.ok_or("missing video intent")?;
            assert_eq!(video.tracks.len(), desired.tracks.len(), "UI traffic retains all video demand");
            for track in video.tracks {
                let previous = desired.tracks.iter().find(|previous| previous.track_id == track.track_id).ok_or("UI traffic replaced unchanged video demand")?;
                let options = track.options.ok_or("missing video options")?;
                let previous = previous.options.as_ref().ok_or("missing baseline video options")?;
                assert_eq!(options.min_height, previous.min_height);
                assert_eq!(options.min_fps, previous.min_fps);
                assert_eq!(options.priority, previous.priority);
                assert_eq!(options.playout_delay, previous.playout_delay);
            }
        }
        let _: bool = evaluate_json(&bidi, &context, r#"(async () => {
            __meet.mark();
            const stream = __meet.currentVideo().srcObject;
            const track = stream.getVideoTracks()[0];
            stream.removeTrack(track);
            stream.addTrack(track);
            let rejected = false;
            try { await __meet.continuity(); }
            catch (error) { rejected = String(error).includes('remote stream tracks changed'); }
            __meet.assert(rejected, 'oracle rejects same-stream detach/re-add even when restored before sampling');
            await __meet.ready();
            return true;
        })()"#).await?;
        eprintln!("Meet stage: explicit reconnect");
        for _ in 0..2 {
            let count: usize = evaluate_json(&bidi, &context, "__meet.receiverPeers().length").await?;
            click(&driver, "Reconnect").await?;
            let recovered: bool = evaluate_json(&bidi, &context, &format!("__meet.recovered({count})")).await?;
            assert!(recovered);
        }
        let count: usize = evaluate_json(&bidi, &context, "__meet.receiverPeers().length").await?;
        eprintln!("Meet stage: transient loss");
        drop(destination);
        let _: bool = evaluate_json(&bidi, &context, r#"(async () => {
            await __meet.wait(() => __meet.receiverPeers().some(peer => peer.connectionState === 'disconnected' || peer.connectionState === 'failed'), 'native detected transient transport loss');
            return true;
        })()"#).await?;
        let _replacement = DestinationServer::start()?;
        let recovered: bool = evaluate_json(&bidi, &context, &format!("__meet.recovered({count})")).await?;
        assert!(recovered);
        let _: bool = evaluate_json(&bidi, &context, r#"(async () => {
            await __meet.chat.publish({sender:'meet-sender', text:'chat after loss', id:crypto.randomUUID(), ts:Date.now()});
            await __meet.reactions.publish({sender:'meet-sender', emoji:'👏', id:crypto.randomUUID(), ts:Date.now()});
            await __meet.wait(() => document.body.innerText.includes('chat after loss') && document.querySelector('.meet-reaction[aria-label="👏"]'), 'both retained subscriptions after detected interruption');
            __meet.mark();
            return true;
        })()"#).await?;
        let _: Vec<Vec<u8>> = evaluate_json(&bidi, &context, "__meet.continuity()").await?;
        driver.find(By::XPath("//button[contains(., 'Leave')]")).await?.click().await?;
        let _: bool = evaluate_json(&bidi, &context, r#"(async () => {
            await __meet.wait(() => document.querySelector('.meet-lobby'), 'leave returns to lobby');
            __meet.assert(__meet.audioNodes().length === 0, 'leave stops remote output');
            await __meet.stopPublisher();
            __meet.assert(__meet.rejections.length === 0, 'cleanup has no unhandled completion');
            return true;
        })()"#).await?;
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    }).await;
    if result.is_err() {
        for path in [
            browser_log.clone(),
            browser_log.with_file_name("destination-server.log"),
        ] {
            if let Ok(log) = tokio::fs::read_to_string(&path).await {
                eprintln!("Meet {}:\n{log}", path.display());
            }
        }
    }
    result.map_err(|error| format!("Meet media contract failed: {error}").into())
}
