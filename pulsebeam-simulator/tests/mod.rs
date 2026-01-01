mod common;

use pulsebeam_agent::actor::AgentBuilder;
use pulsebeam_agent::actor::AgentEvent;
use pulsebeam_agent::signaling::HttpSignalingClient;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use pulsebeam_core::net::UdpSocket;
use pulsebeam_runtime::net::UdpMode;
use std::net::IpAddr;
use std::net::SocketAddr;
use test_strategy::{Arbitrary, proptest};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Arbitrary, Debug)]
struct TestInputStruct {}

#[proptest]
fn my_test(_input: TestInputStruct) {
    common::setup_tracing();
    let shutdown = CancellationToken::new();
    let mut sim = turmoil::Builder::new().build();
    let sender_ip: IpAddr = "192.168.0.16".parse().unwrap();
    let receiver_ip: IpAddr = "192.168.0.17".parse().unwrap();
    let server_ip: IpAddr = "192.168.0.1".parse().unwrap();
    let server_token = shutdown.child_token();

    sim.host(server_ip, move || {
        let server_token = server_token.clone();
        async move {
            let rtc_port = 3478;
            let external_addr: SocketAddr = format!("{}:3478", server_ip).parse()?;
            let local_addr: SocketAddr = format!("0.0.0.0:{}", rtc_port).parse()?;
            let http_api_addr: SocketAddr = "0.0.0.0:3000".parse()?;

            tracing::info!("Starting node on {external_addr} (RTC), {http_api_addr} (API)");
            pulsebeam::node::NodeBuilder::new()
                .workers(1)
                .local_addr(local_addr)
                .external_addr(external_addr)
                .with_udp_mode(UdpMode::Scalar)
                .with_http_api(http_api_addr)
                .run(server_token)
                .await?;
            Ok(())
        }
    });

    let client_token = shutdown.child_token();
    let receiver_token = client_token.clone();
    sim.client(receiver_ip, async move {
        let client = common::create_http_client();
        let server_base_uri = format!("http://{}:3000", server_ip);
        let signaling = HttpSignalingClient::new(client, server_base_uri);
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let mut agent = AgentBuilder::new(signaling, socket)
            .with_local_ip(receiver_ip)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .join("demo")
            .await?;

        while let Some(event) = agent.next_event().await {
            match event {
                AgentEvent::Connected => {
                    tracing::info!("receiver connected");
                }
                AgentEvent::ReceiverAdded(_) => {
                    receiver_token.cancel();
                    return Ok(());
                }
                _ => {}
            }
        }

        panic!("expect track added")
    });

    let sender_token = client_token.clone();
    sim.client(sender_ip, async move {
        let client = common::create_http_client();
        let server_base_uri = format!("http://{}:3000", server_ip);
        let signaling = HttpSignalingClient::new(client, server_base_uri);
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let mut agent = AgentBuilder::new(signaling, socket)
            .with_local_ip(sender_ip)
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .join("demo")
            .await?;
        let mut join_set = JoinSet::new();

        loop {
            tokio::select! {
                Some(event) = agent.next_event() => {
                    match event {
                        AgentEvent::Connected => {
                            tracing::info!("sender connected");
                        }
                        AgentEvent::SenderAdded(sender) => {
                            let looper = common::create_h264_looper(30);
                            join_set.spawn(looper.run(sender));
                            break;
                        }
                        _ => {}
                    }
                }
                _ = sender_token.cancelled() => {
                    break;
                }
                else => break,
            }
        }

        join_set.abort_all();
        Ok(())
    });

    sim.run().unwrap();
}
