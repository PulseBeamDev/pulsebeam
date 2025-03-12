use http::header::{ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE};
use std::net::SocketAddr;
use tracing::info;

use http::Method;
use std::time::Duration;
use tonic::{service::LayerExt, transport::Server};
use tower_http::cors::{AllowOrigin, CorsLayer};

const SERVER_CONFIG: pulsebeam_server_foss::ServerConfig = pulsebeam_server_foss::ServerConfig {
    max_capacity: 65536,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cors = CorsLayer::new()
        .allow_methods([Method::POST])
        .allow_headers([CONTENT_TYPE, ACCEPT_ENCODING, AUTHORIZATION])
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));
    let server = pulsebeam_server_foss::Server::new(SERVER_CONFIG);
    let server = tower::ServiceBuilder::new()
        .layer(cors)
        .layer(tonic_web::GrpcWebLayer::new())
        .into_inner()
        .named_layer(pulsebeam_server_foss::SignalingServer::new(server));

    let addr: SocketAddr = "[::]:3000".parse().unwrap();
    info!("Listening on {addr}");
    Server::builder()
        // GrpcWeb is over http1 so we must enable it.
        .accept_http1(true)
        .add_service(server)
        .serve(addr)
        .await?;

    Ok(())
}
