mod common;

use pulsebeam_runtime::net::UdpMode;
use std::net::SocketAddr;
use test_strategy::{Arbitrary, proptest};
use tokio_util::sync::CancellationToken;

#[derive(Arbitrary, Debug)]
struct TestInputStruct {
    #[strategy(1..10u32)]
    a: u32,

    #[strategy(0..#a)]
    b: u32,
}

#[proptest]
fn my_test(input: TestInputStruct) {
    assert_eq!(common::add(input.a, input.b), input.a + input.b);
    let shutdown = CancellationToken::new();
    let mut sim = turmoil::Builder::new().build();

    sim.host("server", move || {
        let shutdown = shutdown.child_token();
        async move {
            let rtc_port = 3478;
            let external_addr: SocketAddr = format!("{}:{}", "127.0.0.1", rtc_port).parse()?;
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

    sim.client("client", async move {
        use pulsebeam_agent::actor::AgentBuilder;
        use pulsebeam_agent::signaling::HttpSignalingClient;
        use pulsebeam_agent::{MediaKind, TransceiverDirection};
        use pulsebeam_core::net::UdpSocket;

        let client = common::create_http_client();
        let signaling = HttpSignalingClient::new(client, "http://localhost:3000");
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let mut agent = AgentBuilder::new(signaling, socket)
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .join("demo")
            .await;
        Ok(())
    });
}
