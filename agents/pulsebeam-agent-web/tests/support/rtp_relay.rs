use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct RtpPeaks {
    browser_to_sfu: usize,
    sfu_to_browser: usize,
}

impl RtpPeaks {
    fn record(&mut self, packet: &[u8], from_sfu: bool) {
        // STUN, DTLS and RTCP are relayed but are not RTP evidence.
        if let [first, second, ..] = packet
            && first >> 6 == 2
            && !(64..=95).contains(&(second & 127))
        {
            let peak = if from_sfu {
                &mut self.sfu_to_browser
            } else {
                &mut self.browser_to_sfu
            };
            *peak = (*peak).max(packet.len());
        }
    }
}

#[derive(Default)]
pub struct Relays {
    entries: Vec<Relay>,
}

struct Relay {
    task: JoinHandle<()>,
    observations: Arc<Mutex<RtpPeaks>>,
    error: Arc<Mutex<Option<String>>>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Relays {
    pub async fn allocate(&mut self, destination: SocketAddr) -> io::Result<Vec<u8>> {
        // Match the owned server's local interface: Chrome need not gather a
        // loopback candidate, and UDP replies must use the advertised source IP.
        let socket = UdpSocket::bind((destination.ip(), 0)).await?;
        let address = socket.local_addr()?;
        let port = address.port();
        let observations = Arc::new(Mutex::new(RtpPeaks::default()));
        let captured = Arc::clone(&observations);
        let error = Arc::new(Mutex::new(None));
        let failure = Arc::clone(&error);
        let task = tokio::spawn(async move {
            let result: io::Result<()> = async {
                let mut browser = None;
                let mut buffer = vec![0_u8; 65_536];
                loop {
                    let (size, source) = socket.recv_from(&mut buffer).await?;
                    let packet = buffer
                        .get(..size)
                        .ok_or_else(|| io::Error::other("oversized datagram"))?;
                    if source == destination {
                        if let Some(browser) = browser {
                            captured.lock().await.record(packet, true);
                            socket.send_to(packet, browser).await?;
                        }
                    } else if browser.is_none_or(|peer| peer == source) {
                        browser = Some(source);
                        captured.lock().await.record(packet, false);
                        socket.send_to(packet, destination).await?;
                    }
                }
            }
            .await;
            if let Err(error) = result {
                *failure.lock().await = Some(error.to_string());
            }
        });
        let id = self.entries.len();
        self.entries.push(Relay {
            task,
            observations,
            error,
        });
        Ok(serde_json::to_vec(
            &serde_json::json!({"id": id, "address": address.ip().to_string(), "port": port}),
        )?)
    }

    async fn healthy(&self, id: usize) -> io::Result<&Relay> {
        let relay = self
            .entries
            .get(id)
            .ok_or_else(|| io::Error::other("unknown relay"))?;
        if let Some(error) = relay.error.lock().await.as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        Ok(relay)
    }

    pub async fn peaks(&self, id: usize) -> io::Result<Vec<u8>> {
        let relay = self.healthy(id).await?;
        Ok(serde_json::to_vec(&*relay.observations.lock().await)?)
    }
}

#[cfg(all(test, feature = "browser-tests"))]
mod tests {

    #[tokio::test]
    async fn rtp_relay_shutdown_cancels_open_http_handlers() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;
        let server = super::super::StaticServer::start(".").await.unwrap();
        let mut hanging = TcpStream::connect(&server.address).await.unwrap();
        hanging
            .write_all(b"POST /rooms/sender-stats/ HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut allocation = TcpStream::connect(&server.address).await.unwrap();
        allocation
            .write_all(b"GET /__test/rtp-relay?destination=127.0.0.1:9 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        allocation.read_to_string(&mut response).await.unwrap();
        let body: serde_json::Value =
            serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        let port = body["port"].as_u64().unwrap();
        drop(server);
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut byte = [0];
            assert_eq!(hanging.read(&mut byte).await.unwrap(), 0);
            loop {
                if tokio::net::UdpSocket::bind(format!("127.0.0.1:{port}"))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn rtp_relay_tracks_directional_rtp_peaks_only() {
        let mut peaks = super::RtpPeaks::default();
        let mut packet = [0_u8; 32];
        packet[0] = 0x80;
        packet[1] = 96;
        peaks.record(&packet, false);
        peaks.record(&packet[..16], true);
        packet[1] = 200;
        peaks.record(&packet, true);
        assert_eq!(peaks.browser_to_sfu, 32);
        assert_eq!(peaks.sfu_to_browser, 16);
    }
}
