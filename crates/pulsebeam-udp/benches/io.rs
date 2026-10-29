//! Single-owner loopback I/O measurements, not a correctness/latency gate.
use pulsebeam_udp::{BindOptions, Mode, SendDatagram, UdpSocket};
use std::{
    hint::black_box,
    io::{self, Write},
    num::NonZeroU16,
    time::{Duration, Instant},
};

mod allocations {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        sync::atomic::Ordering,
    };

    #[allow(
        clippy::disallowed_types,
        reason = "benchmark-only allocation instrumentation, not shared dataplane state"
    )]
    use std::sync::atomic::AtomicU64;

    pub struct Counted;
    #[allow(
        clippy::disallowed_types,
        reason = "benchmark-only allocation instrumentation, not shared dataplane state"
    )]
    static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

    // SAFETY: allocation/deallocation are delegated unchanged to System. The
    // counter cannot allocate and is diagnostic only, with no lifetime role.
    unsafe impl GlobalAlloc for Counted {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            // SAFETY: caller supplies the GlobalAlloc layout contract.
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: pointers/layouts are passed unchanged to their allocator.
            unsafe { System.dealloc(pointer, layout) }
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            // SAFETY: caller supplies the GlobalAlloc reallocation contract.
            unsafe { System.realloc(pointer, layout, size) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            // SAFETY: caller supplies the GlobalAlloc layout contract.
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    pub fn count() -> u64 {
        ALLOCATIONS.load(Ordering::Relaxed)
    }
}

#[global_allocator]
static ALLOCATOR: allocations::Counted = allocations::Counted;

fn cpu_ticks() -> io::Result<u64> {
    let stat = std::fs::read_to_string("/proc/thread-self/stat")?;
    let (_, fields) = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("invalid thread CPU accounting"))?;
    let mut fields = fields.split_whitespace();
    let user: u64 = fields
        .nth(11)
        .ok_or_else(|| io::Error::other("missing user CPU time"))?
        .parse()
        .map_err(io::Error::other)?;
    let system: u64 = fields
        .next()
        .ok_or_else(|| io::Error::other("missing system CPU time"))?
        .parse()
        .map_err(io::Error::other)?;
    Ok(user.saturating_add(system))
}

fn percentile(sorted: &mut [u128], numerator: usize) -> u128 {
    sorted.sort_unstable();
    let index = sorted
        .len()
        .saturating_mul(numerator)
        .div_ceil(10_000)
        .saturating_sub(1);
    sorted.get(index).copied().unwrap_or_default()
}

async fn socket(mode: Mode) -> io::Result<UdpSocket> {
    let mut options = BindOptions::new(std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
    options.mode = mode;
    options.max_datagram_bytes = 4096;
    pulsebeam_udp::bind(options).await?.activate()
}

async fn exchange(
    sender: &mut UdpSocket,
    receiver: &mut UdpSocket,
    entries: &[SendDatagram<'_>],
) -> io::Result<(u128, u128)> {
    sender.writable().await?;
    let start = Instant::now();
    let report = sender.try_send_batch(entries);
    let send_ns = start.elapsed().as_nanos();
    if report.accepted != entries.len() || report.stop.is_some() {
        return Err(io::Error::other(format!(
            "benchmark local submission stopped: {report:?}"
        )));
    }
    let expected: usize = entries
        .iter()
        .map(|entry| {
            entry.gso_segment_size.map_or(1, |size| {
                entry.payload.len().div_ceil(usize::from(size.get()))
            })
        })
        .sum();
    let mut received = 0usize;
    let mut recv_ns = 0u128;
    while received < expected {
        receiver.readable().await?;
        let start = Instant::now();
        match receiver.try_recv_batch() {
            Ok(batch) => {
                for datagram in batch.iter() {
                    black_box(datagram.payload);
                    received = received.saturating_add(1);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
        recv_ns = recv_ns.saturating_add(start.elapsed().as_nanos());
    }
    if received != expected {
        return Err(io::Error::other("unexpected loopback datagrams"));
    }
    Ok((send_ns, recv_ns))
}

async fn measure(
    mode: Mode,
    segments: &[usize],
    samples: usize,
    ticks_per_sec: f64,
) -> io::Result<()> {
    let mut sender = socket(mode).await?;
    let mut receiver = socket(mode).await?;
    if segments
        .iter()
        .any(|count| *count > sender.capabilities().max_gso_segments)
    {
        return Ok(());
    }
    // Fixed synthetic workload: small RTP, full-sized RTP, and larger DTLS
    // datagrams. GSO uses full segments plus an exact shorter tail.
    let sizes = [80usize, 1200, 2048, 900];
    let payloads: Vec<_> = segments
        .iter()
        .zip(sizes.into_iter().cycle())
        .enumerate()
        .map(|(index, (count, size))| {
            let size = if *count > 1 { 800 } else { size };
            vec![
                u8::try_from(index).unwrap_or(u8::MAX);
                if *count == 1 {
                    size
                } else {
                    size.saturating_mul(*count).saturating_sub(37)
                }
            ]
        })
        .collect();
    let entries: Vec<_> = payloads
        .iter()
        .zip(segments)
        .map(|(payload, count)| SendDatagram {
            destination: receiver.local_addr(),
            payload,
            gso_segment_size: if *count > 1 {
                NonZeroU16::new(800)
            } else {
                None
            },
        })
        .collect();
    for _ in 0..1000 {
        exchange(&mut sender, &mut receiver, &entries).await?;
    }
    let mut send_times = Vec::with_capacity(samples);
    let mut recv_times = Vec::with_capacity(samples);
    let cpu_start = cpu_ticks()?;
    let allocations_start = allocations::count();
    let start = Instant::now();
    for _ in 0..samples {
        let (send, recv) = exchange(&mut sender, &mut receiver, &entries).await?;
        send_times.push(send);
        recv_times.push(recv);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let allocations = allocations::count().saturating_sub(allocations_start);
    let cpu_seconds = cpu_ticks()?.saturating_sub(cpu_start) as f64 / ticks_per_sec;
    let packets = samples.saturating_mul(segments.iter().sum());
    let bytes: usize = payloads
        .iter()
        .map(Vec::len)
        .sum::<usize>()
        .saturating_mul(samples);
    let send_p99 = percentile(&mut send_times, 9900);
    let send_p9999 = percentile(&mut send_times, 9999);
    let recv_p99 = percentile(&mut recv_times, 9900);
    let recv_p9999 = percentile(&mut recv_times, 9999);
    writeln!(
        io::stdout().lock(),
        "mode={mode:?} entries={} segments={segments:?} samples={samples} packets={packets} packets_per_sec={:.0} bytes_per_sec={:.0} thread_cpu_sec={cpu_seconds:.3} cpu_ns_per_packet={:.1} allocs_per_packet={:.5} send_p99_ns={send_p99} send_p9999_ns={send_p9999} recv_p99_ns={recv_p99} recv_p9999_ns={recv_p9999}",
        entries.len(),
        packets as f64 / elapsed,
        bytes as f64 / elapsed,
        cpu_seconds * 1e9 / packets as f64,
        allocations as f64 / packets as f64
    )?;
    Ok(())
}

fn main() -> io::Result<()> {
    let samples = std::env::args()
        .nth(1)
        .map_or(Ok(100_000), |arg| arg.parse::<usize>())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if samples == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "samples must be positive",
        ));
    }
    let hz = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()?;
    let ticks_per_sec: f64 = std::str::from_utf8(&hz.stdout)
        .map_err(io::Error::other)?
        .trim()
        .parse()
        .map_err(io::Error::other)?;
    if !hz.status.success() || ticks_per_sec <= 0.0 {
        return Err(io::Error::other("getconf CLK_TCK failed"));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        for mode in [Mode::Scalar, Mode::Batch] {
            for segments in [
                vec![1],
                vec![1; 8],
                vec![1; 32],
                vec![4],
                vec![16],
                vec![64],
                vec![1, 4, 1, 16, 1, 64, 1, 4],
            ] {
                tokio::time::timeout(
                    Duration::from_secs(120),
                    measure(mode, &segments, samples, ticks_per_sec),
                )
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "loopback benchmark stalled or lost packets",
                    )
                })??;
            }
        }
        Ok(())
    })
}
