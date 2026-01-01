mod common;

use pulsebeam_agent::actor::AgentEvent;
use pulsebeam_runtime::net::UdpMode;
use std::net::SocketAddr;
use test_strategy::{Arbitrary, proptest};
use tokio_util::sync::CancellationToken;

#[derive(Arbitrary, Debug)]
struct TestInputStruct {}

#[proptest]
fn my_test(_input: TestInputStruct) {
    common::setup_tracing();
    let shutdown = CancellationToken::new();
    let mut sim = turmoil::Builder::new().build();
    let client_ip = "192.168.0.16";
    let server_ip = "192.168.0.1";

    sim.host(server_ip, move || {
        let shutdown = shutdown.child_token();
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
                .run(shutdown)
                .await?;
            Ok(())
        }
    });

    sim.client(client_ip, async move {
        use pulsebeam_agent::actor::AgentBuilder;
        use pulsebeam_agent::signaling::HttpSignalingClient;
        use pulsebeam_agent::{MediaKind, TransceiverDirection};
        use pulsebeam_core::net::UdpSocket;

        let client = common::create_http_client();
        let signaling = HttpSignalingClient::new(client, format!("http://{}:3000", server_ip));
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let mut agent = AgentBuilder::new(signaling, socket)
            .with_local_ip(client_ip.parse()?)
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .join("demo")
            .await?;

        while let Some(event) = agent.next_event().await {
            match event {
                AgentEvent::Connected => {
                    return Ok(());
                }
                _ => {}
            }
        }

        unimplemented!();
    });
    sim.run().unwrap();
}
