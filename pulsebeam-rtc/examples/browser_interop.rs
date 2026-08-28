use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    time::{Duration, Instant},
};

use pulsebeam_rtc::{
    ConnectionId, IceCandidate, IceCredentials, IngressPacket, LocalTransport, PacketId,
    PacketProvenance, ServerTransport, TransportMetadata, TransportProtocol, negotiate,
};

#[allow(
    clippy::print_stdout,
    reason = "the Playwright harness observes the fixture server lifecycle"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let http_address = argument("--http-address")?;
    let udp_address = argument("--udp-address")?;
    let listener = TcpListener::bind(http_address)?;
    let udp = UdpSocket::bind(udp_address)?;
    udp.set_read_timeout(Some(Duration::from_millis(10)))?;
    println!("READY {}", listener.local_addr()?);

    let mut connection = accept_connection(&listener, &udp)?;
    let local = udp.local_addr()?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(30))
        .ok_or("browser interoperability deadline overflow")?;
    let mut packet_id = 0u64;
    let mut buffer = [0u8; 2048];

    while Instant::now() < deadline {
        let now = Instant::now();
        if let Ok((len, remote)) = udp.recv_from(&mut buffer) {
            packet_id = packet_id.wrapping_add(1);
            let provenance = PacketProvenance::new(
                now,
                TransportMetadata::new(TransportProtocol::Udp, remote, local),
                PacketId::new(packet_id),
            );
            debug_assert!(len <= buffer.len(), "UDP receive length fits its buffer");
            let bytes = buffer
                .get(..len)
                .ok_or("UDP receive length exceeds its buffer")?;
            connection.handle_datagram(now, IngressPacket::new(bytes, provenance))?;
        }
        connection.handle_timeout(now);
        while let Some(datagram) = connection.poll_egress() {
            udp.send_to(datagram.bytes(), datagram.transport().destination())?;
        }
        while let Some(event) = connection.poll_event() {
            println!("EVENT {event:?}");
        }
        while let Some(packet) = connection.poll_authenticated() {
            println!("AUTHENTICATED {}", packet.bytes().len());
        }
    }

    Ok(())
}

fn argument(name: &str) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    while let Some(flag) = arguments.next() {
        if flag == name {
            return arguments
                .next()
                .ok_or_else(|| format!("missing value for {name}"))?
                .parse()
                .map_err(Into::into);
        }
    }
    Err(format!("missing {name}").into())
}

fn accept_connection(
    listener: &TcpListener,
    udp: &UdpSocket,
) -> Result<pulsebeam_rtc::LiveConnection, Box<dyn std::error::Error>> {
    let (mut stream, _) = listener.accept()?;
    let offer = read_offer(&mut stream)?;
    let ice = IceCredentials::new(
        "pulsebeam".to_owned(),
        "pulsebeam-browser-interop".to_owned(),
    )
    .ok_or("invalid local ICE credentials")?;
    let local = LocalTransport::generate(ice.clone())?;
    let udp_address = udp.local_addr()?;
    let candidate = IceCandidate::new(format!(
        "candidate:1 1 udp 2130706431 {} {} typ host",
        udp_address.ip(),
        udp_address.port()
    ))
    .ok_or("invalid local candidate")?;
    let server = ServerTransport::new(1, ice, local.fingerprint().clone(), Box::new([candidate]));
    let result = negotiate(&offer, &server)?;
    write_answer(&mut stream, result.answer().as_str())?;
    Ok(pulsebeam_rtc::LiveConnection::new(
        ConnectionId::new(1),
        result.session().clone(),
        local,
        Instant::now(),
    )?)
}

fn read_offer(stream: &mut TcpStream) -> Result<String, Box<dyn std::error::Error>> {
    let mut bytes = Vec::with_capacity(4096);
    let mut buffer = [0u8; 2048];
    let header_end = loop {
        let len = stream.read(&mut buffer)?;
        if len == 0 {
            return Err("signaling connection closed before request body".into());
        }
        debug_assert!(len <= buffer.len(), "TCP read length fits its buffer");
        let received = buffer
            .get(..len)
            .ok_or("TCP read length exceeds its buffer")?;
        bytes.extend_from_slice(received);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end.checked_add(4).ok_or("HTTP header length overflow")?;
        }
    };
    let headers = std::str::from_utf8(
        bytes
            .get(..header_end)
            .ok_or("HTTP header length exceeds request")?,
    )?;
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .ok_or("missing Content-Length")?
        .parse::<usize>()?;
    while bytes.len().saturating_sub(header_end) < length {
        let len = stream.read(&mut buffer)?;
        if len == 0 {
            return Err("signaling connection closed before complete request body".into());
        }
        debug_assert!(len <= buffer.len(), "TCP read length fits its buffer");
        let received = buffer
            .get(..len)
            .ok_or("TCP read length exceeds its buffer")?;
        bytes.extend_from_slice(received);
    }
    let body_end = header_end
        .checked_add(length)
        .ok_or("HTTP request body length overflow")?;
    let body = bytes
        .get(header_end..body_end)
        .ok_or("HTTP request body exceeds received bytes")?;
    Ok(std::str::from_utf8(body)?.to_owned())
}

fn write_answer(stream: &mut TcpStream, answer: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/sdp\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
        answer.len()
    )
}
