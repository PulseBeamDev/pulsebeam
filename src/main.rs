use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::fmt::format::FmtSpan;

use pulsebeam_server_foss::server::Server;
use pulsebeam_server_foss::{manager::IndexManager, proto::signaling_server::SignalingServer};
use std::time::Duration;
use tonic::service::LayerExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

const CONNECTION_CAPACITY: u64 = 65536;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .compact()
        .with_span_events(FmtSpan::NONE)
        .with_env_filter(env_filter)
        .init();

    let cors = CorsLayer::very_permissive()
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));
    let token = CancellationToken::new();
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<SignalingServer<crate::Server<IndexManager>>>()
        .await;
    // https://github.com/hyperium/tonic/discussions/1784
    // switch to v1 after tooling supports it
    let reflector = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(pulsebeam_server_foss::proto::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
        .build_v1()?;
    let server = Server::spawn_default(token, CONNECTION_CAPACITY);
    let grpc_server = tower::ServiceBuilder::new()
        .layer(cors)
        .layer(tonic_web::GrpcWebLayer::new())
        .into_inner()
        .named_layer(SignalingServer::new(server));

    let addr: SocketAddr = "[::]:3000".parse().unwrap();
    info!("Listening on {addr}");

    tonic::transport::Server::builder()
        .accept_http1(true)
        .layer(tower_http::trace::TraceLayer::new_for_grpc())
        .add_service(health_service)
        .add_service(reflector)
        .add_service(grpc_server)
        .serve(addr)
        .await?;

    Ok(())
}
