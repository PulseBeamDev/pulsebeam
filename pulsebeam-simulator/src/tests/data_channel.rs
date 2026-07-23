use super::common;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

#[test]
fn data_channel_pubsub_forwarding_test() -> turmoil::Result {
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

            client.ctx.driver.declare_subscribe_topic(&topic, None)?;
            client
                .drive_with(|ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&(topic.clone(), None))
                    else {
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
fn data_channel_latency_regression_test() -> turmoil::Result {
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

            client.ctx.driver.declare_subscribe_topic(&topic, None)?;
            client
                .drive_with_interval(Duration::from_micros(100), |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&(topic.clone(), None))
                    else {
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

/// Two publishers on the same topic; a subscriber scoped to each publisher
/// must only ever see that publisher's payloads, while a wildcard
/// (unscoped) subscriber must still see both — verifying the scoped
/// subscribe grammar filters by origin without breaking the existing
/// all-publishers fan-in behavior.
#[test]
fn data_channel_scoped_subscribe_routing_test() -> turmoil::Result {
    let mut sim = turmoil::Builder::new()
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0x5C0BED11)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub_a_ip = common::subnet_ip(subnet, 2);
    let pub_b_ip = common::subnet_ip(subnet, 3);
    let scoped_a_ip = common::subnet_ip(subnet, 4);
    let scoped_b_ip = common::subnet_ip(subnet, 5);
    let aggregate_ip = common::subnet_ip(subnet, 6);

    let topic = "scoped_topic".to_string();
    let payload_a = b"payload-from-a".to_vec();
    let payload_b = b"payload-from-b".to_vec();

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xC0FFEE00))
            .await
            .map_err(|e| e.into())
    });

    let pub_a_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let pub_b_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    #[derive(Default)]
    struct Flags {
        scoped_a_ok: bool,
        scoped_b_ok: bool,
        aggregate_saw_a: bool,
        aggregate_saw_b: bool,
    }
    impl Flags {
        fn all_done(&self) -> bool {
            self.scoped_a_ok && self.scoped_b_ok && self.aggregate_saw_a && self.aggregate_saw_b
        }
    }
    let flags = Arc::new(Mutex::new(Flags::default()));

    // Publisher A: keeps sending until every subscriber has reached its goal.
    {
        let topic = topic.clone();
        let payload_a = payload_a.clone();
        let pub_a_id = pub_a_id.clone();
        let flags = flags.clone();
        sim.client(pub_a_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_a_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            *pub_a_id.lock().unwrap() = Some(client.ctx.driver.participant_id().clone());
            client.ctx.driver.declare_publish_topic(&topic)?;
            client
                .drive_with(move |ctx| {
                    if let Some(publisher) = ctx.published_topics.get_mut(&topic) {
                        let _ = publisher.try_send(payload_a.clone());
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    // Publisher B: same, with a distinguishable payload.
    {
        let topic = topic.clone();
        let payload_b = payload_b.clone();
        let pub_b_id = pub_b_id.clone();
        let flags = flags.clone();
        sim.client(pub_b_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_b_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            *pub_b_id.lock().unwrap() = Some(client.ctx.driver.participant_id().clone());
            client.ctx.driver.declare_publish_topic(&topic)?;
            client
                .drive_with(move |ctx| {
                    if let Some(publisher) = ctx.published_topics.get_mut(&topic) {
                        let _ = publisher.try_send(payload_b.clone());
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    // Subscriber scoped to publisher A: must only ever observe A's payload.
    {
        let topic = topic.clone();
        let payload_a = payload_a.clone();
        let payload_b = payload_b.clone();
        let pub_a_id = pub_a_id.clone();
        let flags = flags.clone();
        sim.client(scoped_a_ip, async move {
            let target_id = loop {
                if let Some(id) = pub_a_id.lock().unwrap().clone() {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            };

            let mut client = common::client::SimClientBuilder::bind(scoped_a_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            client
                .ctx
                .driver
                .declare_subscribe_topic(&topic, Some(&target_id))?;
            let key = (topic.clone(), Some(target_id));
            client
                .drive_with(move |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&key) else {
                        return false;
                    };
                    while let Ok(payload) = subscriber.try_recv() {
                        assert_ne!(
                            payload, payload_b,
                            "subscriber scoped to publisher A must never receive B's payload"
                        );
                        assert_eq!(payload, payload_a);
                        flags.lock().unwrap().scoped_a_ok = true;
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    // Subscriber scoped to publisher B: symmetric to the above.
    {
        let topic = topic.clone();
        let payload_a = payload_a.clone();
        let payload_b = payload_b.clone();
        let pub_b_id = pub_b_id.clone();
        let flags = flags.clone();
        sim.client(scoped_b_ip, async move {
            let target_id = loop {
                if let Some(id) = pub_b_id.lock().unwrap().clone() {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            };

            let mut client = common::client::SimClientBuilder::bind(scoped_b_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            client
                .ctx
                .driver
                .declare_subscribe_topic(&topic, Some(&target_id))?;
            let key = (topic.clone(), Some(target_id));
            client
                .drive_with(move |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&key) else {
                        return false;
                    };
                    while let Ok(payload) = subscriber.try_recv() {
                        assert_ne!(
                            payload, payload_a,
                            "subscriber scoped to publisher B must never receive A's payload"
                        );
                        assert_eq!(payload, payload_b);
                        flags.lock().unwrap().scoped_b_ok = true;
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    // Wildcard (unscoped) subscriber: must see both A's and B's payloads,
    // regression-checking the pre-existing all-publishers fan-in behavior.
    {
        let topic = topic.clone();
        let payload_a = payload_a.clone();
        let payload_b = payload_b.clone();
        let flags = flags.clone();
        sim.client(aggregate_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(aggregate_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            client.ctx.driver.declare_subscribe_topic(&topic, None)?;
            let key = (topic.clone(), None);
            client
                .drive_with(move |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&key) else {
                        return false;
                    };
                    while let Ok(payload) = subscriber.try_recv() {
                        if payload == payload_a {
                            flags.lock().unwrap().aggregate_saw_a = true;
                        } else if payload == payload_b {
                            flags.lock().unwrap().aggregate_saw_b = true;
                        }
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    sim.run().unwrap();
    Ok(())
}
