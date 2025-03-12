use std::net::SocketAddr;
use tracing::info;

use std::time::Duration;
use tonic::{service::LayerExt, transport::Server};
use tower_http::cors::{AllowOrigin, CorsLayer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cors = CorsLayer::very_permissive()
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));
    let server = pulsebeam_server_foss::Server::default();
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
        .layer(tower_http::trace::TraceLayer::new_for_grpc())
        .add_service(server)
        .serve(addr)
        .await?;

    Ok(())
}
