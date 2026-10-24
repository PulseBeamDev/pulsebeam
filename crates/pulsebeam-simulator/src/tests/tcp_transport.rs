use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;

use super::common::client::create_http_client;

#[test]
fn pooled_http_post_survives_partition_without_duplicate_admission() {
    let _clocks = crate::sim_clock::SimClocksGuard::init();
    let connections = Arc::new(AtomicUsize::new(0));
    let admissions = Arc::new(AtomicUsize::new(0));
    let mut sim = turmoil::Builder::new()
        .rng_seed(0x7c9)
        .simulation_duration(Duration::from_secs(10))
        .build();
    let accepted = connections.clone();
    let admitted = admissions.clone();
    sim.host("server", move || {
        let accepted = accepted.clone();
        let admitted = admitted.clone();
        async move {
            let listener = turmoil::net::TcpListener::bind("0.0.0.0:80").await?;
            loop {
                let (stream, _) = listener.accept().await?;
                accepted.fetch_add(1, Ordering::SeqCst);
                let admitted = admitted.clone();
                tokio::spawn(async move {
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request: Request<Incoming>| {
                                let admitted = admitted.clone();
                                async move {
                                    let post = request.method() == http::Method::POST;
                                    let body = request.into_body().collect().await?.to_bytes();
                                    if post {
                                        assert_eq!(body.as_ref(), b"one admission");
                                        admitted.fetch_add(1, Ordering::SeqCst);
                                    }
                                    Ok::<_, hyper::Error>(Response::new(Full::new(
                                        Bytes::from_static(b"ok"),
                                    )))
                                }
                            }),
                        )
                        .await
                        .unwrap();
                });
            }
        }
    });
    sim.client("client", async move {
        let http = create_http_client();
        let request = |method| {
            http::Request::builder()
                .method(method)
                .uri("http://server:80/")
                .body(b"one admission".to_vec())
                .unwrap()
        };
        http.execute(request(http::Method::GET)).await.unwrap();
        tokio::task::yield_now().await;
        turmoil::partition("client", "server");
        let post = async {
            let response = http.execute(request(http::Method::POST)).await.unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            assert_eq!(response.body(), b"ok");
        };
        let repair = async {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert_eq!(admissions.load(Ordering::SeqCst), 0);
            turmoil::repair("client", "server");
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(post, repair);
        })
        .await
        .expect("established TCP did not retransmit the POST after repair");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "POST bypassed the pooled connection"
        );
        assert_eq!(
            admissions.load(Ordering::SeqCst),
            1,
            "POST was admitted more than once"
        );
        Ok::<_, Box<dyn std::error::Error>>(())
    });
    sim.run().unwrap();
}

#[test]
fn retransmitted_fin_drains_after_receive_backpressure() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let _clocks = crate::sim_clock::SimClocksGuard::init();
    let received = Arc::new(AtomicUsize::new(0));
    let mut sim = turmoil::Builder::new()
        .rng_seed(0x7c9)
        .tcp_capacity(1)
        .simulation_duration(Duration::from_secs(10))
        .build();
    let observed = received.clone();
    sim.host("server", move || {
        let observed = observed.clone();
        async move {
            let listener = turmoil::net::TcpListener::bind("0.0.0.0:80").await?;
            let (mut stream, _) = listener.accept().await?;
            // One Data fills the receive channel. FIN must wait in reassembly,
            // then drain on retransmission after the application frees capacity.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await?;
            assert_eq!(bytes, b"ordered exactly once");
            observed.store(bytes.len(), Ordering::SeqCst);
            Ok(())
        }
    });
    sim.client("client", async move {
        let mut stream = turmoil::net::TcpStream::connect("server:80").await?;
        stream.write_all(b"ordered exactly once").await?;
        stream.shutdown().await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while received.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("FIN remained stuck after the receive channel drained");
        Ok(())
    });
    sim.run().unwrap();
}
