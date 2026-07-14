use super::common;
use std::time::Duration;
use tokio::time::Instant;

#[test]
fn data_channel_pubsub_forwarding_test() -> turmoil::Result {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0x0BADC0DE)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub_ip = common::subnet_ip(subnet, 2);
    let sub_ip = common::subnet_ip(subnet, 3);

    let topic = "sim_topic".to_string();
    let payload = b"hello-data-channel".to_vec();

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(|e| e.into())
    });

    {
        let topic = topic.clone();
        let payload = payload.clone();
        sim.client(pub_ip, async move {
            let result: anyhow::Result<()> = async {
                let mut client = common::client::SimClientBuilder::bind(pub_ip, server_ip)
                    .await?
                    .connect("room-data")
                    .await?;

                client.ctx.driver.ensure_publish_topic(&topic).await?;
                client.drive_for(Duration::from_secs(2)).await?;

                let deadline = Instant::now() + Duration::from_secs(15);
                let mut sent_count = 0usize;
                while Instant::now() < deadline {
                    if client.ctx.driver.publish_data(&topic, &payload) {
                        sent_count += 1;
                    }
                    let _ = tokio::time::timeout(
                        Duration::from_millis(50),
                        client.ctx.driver.poll(),
                    )
                    .await;
                }

                if sent_count == 0 {
                    anyhow::bail!("publisher failed to send data before timeout");
                }

                client.drive_for(Duration::from_secs(2)).await?;
                Ok(())
            }
            .await;

            result.map_err(|e| e.into())
        });
    }

    {
        let topic = topic.clone();
        let payload = payload.clone();
        sim.client(sub_ip, async move {
            let result: anyhow::Result<()> = async {
                let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
                    .await?
                    .connect("room-data")
                    .await?;

                client.ctx.driver.ensure_subscribe_topic(&topic).await?;
                client.drive_for(Duration::from_secs(2)).await?;

                client
                    .drive_until(Duration::from_secs(20), |ctx| {
                        ctx.received_data
                            .iter()
                            .any(|(t, p)| t == &topic && p.as_slice() == payload.as_slice())
                    })
                    .await?;

                Ok(())
            }
            .await;

            result.map_err(|e| e.into())
        });
    }

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(30))?;
    Ok(())
}
