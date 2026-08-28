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
    loop {
        let (mut stream, _) = listener.accept()?;
        let Some(offer) = read_offer(&mut stream)? else {
            continue;
        };
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
        let server =
            ServerTransport::new(1, ice, local.fingerprint().clone(), Box::new([candidate]));
        let result = negotiate(&offer, &server)?;
        write_answer(&mut stream, result.answer().as_str())?;
        return Ok(pulsebeam_rtc::LiveConnection::new(
            ConnectionId::new(1),
            result.session().clone(),
            local,
            Instant::now(),
        )?);
    }
}

fn read_offer(stream: &mut TcpStream) -> Result<Option<String>, Box<dyn std::error::Error>> {
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
    if headers
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("OPTIONS "))
    {
        write_preflight(stream)?;
        return Ok(None);
    }
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>())
        })
        .transpose()?;
    let chunked = headers.lines().any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        name.eq_ignore_ascii_case("transfer-encoding")
            && value.trim().eq_ignore_ascii_case("chunked")
    });
    let body = if let Some(length) = length {
        read_fixed_body(stream, &mut bytes, &mut buffer, header_end, length)?
    } else if chunked {
        read_chunked_body(stream, &mut bytes, &mut buffer, header_end)?
    } else {
        return Err("missing request body framing".into());
    };
    Ok(Some(std::str::from_utf8(&body)?.to_owned()))
}

fn read_fixed_body(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    buffer: &mut [u8],
    header_end: usize,
    length: usize,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    while bytes.len().saturating_sub(header_end) < length {
        read_more(stream, bytes, buffer)?;
    }
    let body_end = header_end
        .checked_add(length)
        .ok_or("HTTP request body length overflow")?;
    bytes
        .get(header_end..body_end)
        .map(ToOwned::to_owned)
        .ok_or_else(|| "HTTP request body exceeds received bytes".into())
}

fn read_chunked_body(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    buffer: &mut [u8],
    header_end: usize,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    while !bytes
        .get(header_end..)
        .is_some_and(|body| body.ends_with(b"\r\n0\r\n\r\n"))
    {
        read_more(stream, bytes, buffer)?;
    }
    decode_chunked(
        bytes
            .get(header_end..)
            .ok_or("HTTP chunked body exceeds received bytes")?,
    )
}

fn read_more(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    buffer: &mut [u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let len = stream.read(buffer)?;
    if len == 0 {
        return Err("signaling connection closed before complete request body".into());
    }
    debug_assert!(len <= buffer.len(), "TCP read length fits its buffer");
    let received = buffer
        .get(..len)
        .ok_or("TCP read length exceeds its buffer")?;
    bytes.extend_from_slice(received);
    Ok(())
}

fn decode_chunked(mut input: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut body = Vec::with_capacity(input.len());
    loop {
        let line_end = input
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or("incomplete HTTP chunk size")?;
        let size = std::str::from_utf8(
            input
                .get(..line_end)
                .ok_or("HTTP chunk size exceeds input")?,
        )?
        .split(';')
        .next()
        .ok_or("missing HTTP chunk size")?;
        let size = usize::from_str_radix(size, 16)?;
        let content_start = line_end.checked_add(2).ok_or("HTTP chunk size overflow")?;
        if size == 0 {
            return Ok(body);
        }
        let content_end = content_start
            .checked_add(size)
            .ok_or("HTTP chunk length overflow")?;
        body.extend_from_slice(
            input
                .get(content_start..content_end)
                .ok_or("HTTP chunk exceeds input")?,
        );
        let next_chunk = content_end
            .checked_add(2)
            .ok_or("HTTP chunk delimiter overflow")?;
        if input.get(content_end..next_chunk) != Some(b"\r\n") {
            return Err("invalid HTTP chunk delimiter".into());
        }
        input = input
            .get(next_chunk..)
            .ok_or("HTTP chunk offset exceeds input")?;
    }
}

fn write_answer(stream: &mut TcpStream, answer: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/sdp\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
        answer.len()
    )
}

fn write_preflight(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
}
