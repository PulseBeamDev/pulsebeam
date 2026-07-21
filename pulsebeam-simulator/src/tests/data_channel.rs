use super::common;
use pulsebeam_agent::agent::DeliveryClass;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

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

#[test]
fn data_channel_reliable_ordered_forwarding_test() -> turmoil::Result {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0x0DDBA11)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub_ip = common::subnet_ip(subnet, 2);
    let sub_ip = common::subnet_ip(subnet, 3);

    let topic = "reliable_topic".to_string();
    const TOTAL_MESSAGES: usize = 32;

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(|e| e.into())
    });

    let received_all = Arc::new(Mutex::new(false));
    {
        let topic = topic.clone();
        let received_all = received_all.clone();
        sim.client(pub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_ip, server_ip)
                .await?
                .connect("room-data-reliable")
                .await?;

            client
                .ctx
                .driver
                .declare_publish_topic_with(&topic, DeliveryClass::ReliableOrdered)?;
            // Messages published before the subscriber attaches are not replayed.
            let mut next_seq = 0usize;
            client
                .drive_with(|ctx| {
                    let Some(publisher) = ctx.published_topics.get_mut(&topic) else {
                        return false;
                    };

                    let payload = format!("seq-{next_seq}").into_bytes();
                    if publisher.try_send(payload).is_ok() {
                        next_seq += 1;
                    }
                    *received_all.lock().unwrap()
                })
                .await?;
            Ok(())
        });
    }

    {
        let topic = topic.clone();
        let received_all = received_all.clone();
        sim.client(sub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
                .await?
                .connect("room-data-reliable")
                .await?;

            client
                .ctx
                .driver
                .declare_subscribe_topic_with(&topic, DeliveryClass::ReliableOrdered)?;
            let mut first_seq = None;
            let mut count = 0usize;
            client
                .drive_with(|ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&topic) else {
                        return false;
                    };

                    while let Ok(payload) = subscriber.try_recv() {
                        let text = String::from_utf8(payload).expect("utf8 payload");
                        let seq: usize = text
                            .strip_prefix("seq-")
                            .expect("seq- prefix")
                            .parse()
                            .expect("sequence number");
                        let start = *first_seq.get_or_insert(seq);
                        assert_eq!(
                            seq,
                            start + count,
                            "reliable-ordered topic delivered out of order or dropped"
                        );
                        count += 1;
                    }

                    if count >= TOTAL_MESSAGES {
                        *received_all.lock().unwrap() = true;
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

#[test]
fn data_channel_latency_regression_test() -> turmoil::Result {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0xFEEDBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub_ip = common::subnet_ip(subnet, 2);
    let sub_ip = common::subnet_ip(subnet, 3);

    let topic = "latency_topic".to_string();
    let payload = b"ping".to_vec();

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xABCD1234))
            .await
            .map_err(|e| e.into())
    });

    let sent_at = Arc::new(Mutex::new(None::<Instant>));
    let observed = Arc::new(Mutex::new(None::<Duration>));
    let subscriber_ready = Arc::new(Mutex::new(false));

    {
        let topic = topic.clone();
        let payload = payload.clone();
        let sent_at = sent_at.clone();
        let observed = observed.clone();
        let subscriber_ready = subscriber_ready.clone();
        sim.client(pub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_ip, server_ip)
                .await?
                .connect("room-data-latency")
                .await?;

            client.ctx.driver.declare_publish_topic(&topic)?;
            client
                .drive_with_interval(Duration::from_micros(100), |ctx| {
                    if !*subscriber_ready.lock().unwrap() {
                        return false;
                    }

                    let Some(publisher) = ctx.published_topics.get_mut(&topic) else {
                        return false;
                    };

                    if publisher.try_send(payload.clone()).is_ok() {
                        *sent_at.lock().unwrap() = Some(Instant::now());
                    }

                    observed.lock().unwrap().is_some()
                })
                .await?;
            Ok(())
        });
    }

    {
        let topic = topic.clone();
        let payload = payload.clone();
        let sent_at = sent_at.clone();
        let observed = observed.clone();
        let subscriber_ready = subscriber_ready.clone();
        sim.client(sub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
                .await?
                .connect("room-data-latency")
                .await?;

            client.ctx.driver.declare_subscribe_topic(&topic)?;
            client
                .drive_with_interval(Duration::from_micros(100), |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&topic) else {
                        return false;
                    };
                    *subscriber_ready.lock().unwrap() = true;

                    let Ok(recv_payload) = subscriber.try_recv() else {
                        return false;
                    };

                    if recv_payload != payload {
                        return false;
                    }

                    let Some(started_at) = *sent_at.lock().unwrap() else {
                        return false;
                    };

                    *observed.lock().unwrap() = Some(Instant::now().duration_since(started_at));
                    true
                })
                .await?;
            Ok(())
        });
    }

    sim.run()?;

    let latency = observed
        .lock()
        .unwrap()
        .expect("expected at least one observed data-channel latency sample");

    assert!(
        latency <= Duration::from_millis(1),
        "data-channel latency regression: observed {:?}, expected <= 10ms in simulator",
        latency
    );

    Ok(())
}
