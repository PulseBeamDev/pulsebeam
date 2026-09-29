use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    packets: u64,
    with_extension: u64,
    first_value: Option<Vec<u8>>,
}

#[derive(Default)]
pub struct Relays {
    entries: Vec<Relay>,
}

struct Relay {
    task: JoinHandle<()>,
    observations: Arc<Mutex<HashMap<u32, Observation>>>,
    error: Arc<Mutex<Option<String>>>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Relays {
    pub async fn allocate(
        &mut self,
        destination: SocketAddr,
        extension: u8,
    ) -> io::Result<Vec<u8>> {
        // Match the owned server's local interface: Chrome need not gather a
        // loopback candidate, and UDP replies must use the advertised source IP.
        let socket = UdpSocket::bind((destination.ip(), 0)).await?;
        let address = socket.local_addr()?;
        let port = address.port();
        let observations = Arc::new(Mutex::new(HashMap::<u32, Observation>::new()));
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
                            if let Some((ssrc, value)) =
                                inspect_rtp(packet, extension).map_err(io::Error::other)?
                            {
                                let mut captured = captured.lock().await;
                                let observation = captured.entry(ssrc).or_default();
                                observation.packets = observation.packets.saturating_add(1);
                                if let Some(value) = value {
                                    observation.with_extension =
                                        observation.with_extension.saturating_add(1);
                                    observation
                                        .first_value
                                        .get_or_insert_with(|| value.to_vec());
                                }
                            }
                            socket.send_to(packet, browser).await?;
                        }
                    } else if browser.is_none_or(|peer| peer == source) {
                        browser = Some(source);
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

    pub async fn observation(&self, id: usize, ssrc: u32) -> io::Result<Vec<u8>> {
        let relay = self
            .entries
            .get(id)
            .ok_or_else(|| io::Error::other("unknown relay"))?;
        if let Some(error) = relay.error.lock().await.as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        let observations = relay.observations.lock().await;
        Ok(serde_json::to_vec(
            &observations.get(&ssrc).cloned().unwrap_or_default(),
        )?)
    }
}

// SRTP encrypts the payload, not these negotiated RTP header extensions.
// Non-RTP traffic (STUN, DTLS, RTCP) is forwarded but never counted as evidence.
type PacketExtension<'a> = (u32, Option<&'a [u8]>);

fn inspect_rtp(packet: &[u8], wanted: u8) -> Result<Option<PacketExtension<'_>>, &'static str> {
    let Some(&first) = packet.first() else {
        return Ok(None);
    };
    if first >> 6 != 2 {
        return Ok(None);
    }
    let second = *packet.get(1).ok_or("truncated RTP/RTCP")?;
    if (192..=223).contains(&second) {
        return Ok(None);
    }
    let ssrc = u32::from_be_bytes(
        packet
            .get(8..12)
            .ok_or("truncated RTP header")?
            .try_into()
            .map_err(|_| "invalid SSRC")?,
    );
    let header_len = usize::from(first & 0x0f)
        .saturating_mul(4)
        .saturating_add(12);
    let tail = packet.get(header_len..).ok_or("truncated CSRCs")?;
    if first & 0x10 == 0 {
        return Ok(Some((ssrc, None)));
    }
    let header = tail.get(..4).ok_or("truncated extension header")?;
    let profile = u16::from_be_bytes(
        header
            .get(..2)
            .ok_or("missing profile")?
            .try_into()
            .map_err(|_| "invalid profile")?,
    );
    let words = u16::from_be_bytes(
        header
            .get(2..4)
            .ok_or("missing length")?
            .try_into()
            .map_err(|_| "invalid length")?,
    );
    let mut extensions = tail
        .get(4..usize::from(words).saturating_mul(4).saturating_add(4))
        .ok_or("truncated extensions")?;
    if profile != 0xbede && profile & 0xfff0 != 0x1000 {
        return Err("unsupported RTP extension profile");
    }
    let mut found = None;
    while let Some((&tag, rest)) = extensions.split_first() {
        extensions = rest;
        if tag == 0 {
            continue;
        }
        let (id, size) = if profile == 0xbede {
            if tag >> 4 == 15 {
                break;
            }
            (tag >> 4, usize::from(tag & 0x0f).saturating_add(1))
        } else if profile & 0xfff0 == 0x1000 {
            let (&size, rest) = extensions.split_first().ok_or("missing two-byte length")?;
            extensions = rest;
            (tag, usize::from(size))
        } else {
            return Err("unsupported RTP extension profile");
        };
        let value = extensions.get(..size).ok_or("truncated extension value")?;
        extensions = extensions.get(size..).ok_or("truncated extension value")?;
        if id == wanted {
            if found.is_some() {
                return Err("duplicate playout extension");
            }
            found = Some(value);
        }
    }
    Ok(Some((ssrc, found)))
}

#[cfg(test)]
mod tests {
    use super::inspect_rtp;

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
            .write_all(
                b"GET /__test/rtp-relay?destination=127.0.0.1:9&extension=7 HTTP/1.1\r\n\r\n",
            )
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
    fn rtp_relay_parses_both_extension_formats_and_absence() {
        let mut packet = vec![0x90, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 42];
        packet.extend([0xbe, 0xde, 0, 1, 0x72, 0, 0xa0, 10]);
        assert_eq!(
            inspect_rtp(&packet, 7).unwrap(),
            Some((42, Some(&[0, 0xa0, 10][..])))
        );
        assert_eq!(inspect_rtp(&packet, 8).unwrap(), Some((42, None)));
        packet.truncate(12);
        packet.extend([0x10, 0, 0, 2, 7, 3, 0, 0xa0, 10, 0, 0, 0]);
        assert_eq!(
            inspect_rtp(&packet, 7).unwrap(),
            Some((42, Some(&[0, 0xa0, 10][..])))
        );
        packet.pop();
        assert!(inspect_rtp(&packet, 7).is_err());
        assert_eq!(inspect_rtp(&[22, 0, 0], 7).unwrap(), None);
        assert_eq!(inspect_rtp(&[0x80, 200], 7).unwrap(), None);
        assert_eq!(
            inspect_rtp(&[0x80, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 42], 7).unwrap(),
            Some((42, None))
        );
    }
}
