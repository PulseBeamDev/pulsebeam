use super::common;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[test]
fn data_channel_pubsub_forwarding_test() -> turmoil::Result {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
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

    let received = Arc::new(Mutex::new(false));
    {
        let topic = topic.clone();
        let payload = payload.clone();
        let received = received.clone();
        sim.client(pub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_ip, server_ip)
                .await?
                .connect("room-data")
                .await?;

            client.ctx.driver.declare_publish_topic(&topic)?;
            client
                .drive_with(|ctx| {
                    let Some(publisher) = ctx.published_topics.get_mut(&topic) else {
                        return false;
                    };

                    let _ = publisher.try_send(payload.clone());
                    *received.lock().unwrap()
                })
                .await?;
            Ok(())
        });
    }

    {
        let topic = topic.clone();
        let payload = payload.clone();
        sim.client(sub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
                .await?
                .connect("room-data")
                .await?;

            client.ctx.driver.declare_subscribe_topic(&topic)?;
            client
                .drive_with(|ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&topic) else {
                        return false;
                    };

                    let Ok(recv_payload) = subscriber.try_recv() else {
                        return false;
                    };

                    if recv_payload == payload {
                        *received.lock().unwrap() = true;
                        true
                    } else {
                        false
                    }
                })
                .await?;
            Ok(())
        });
    }

    sim.run().unwrap();
    Ok(())
}
