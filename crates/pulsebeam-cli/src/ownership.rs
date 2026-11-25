use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, ensure};

const MAX_REQUEST_BYTES: u64 = 16 * 1024;
const MAX_RESPONSE_BYTES: u64 = 4096;

#[derive(clap::Args)]
pub(crate) struct OwnershipArgs {
    /// Data directory of the provisioned server running with --operator-socket.
    #[arg(long)]
    data_dir: PathBuf,
}

impl OwnershipArgs {
    pub(crate) fn run(self, input: impl Read, output: impl Write) -> Result<()> {
        let mut request = Vec::new();
        input
            .take(MAX_REQUEST_BYTES + 1)
            .read_to_end(&mut request)?;
        ensure!(
            !request.is_empty() && request.len() as u64 <= MAX_REQUEST_BYTES,
            "invalid ownership request size"
        );
        let stream = UnixStream::connect(self.data_dir.join("operator/ownership.sock"))
            .context("connecting to the local ownership socket")?;
        exchange(stream, &request, output)
    }
}

fn exchange(mut stream: UnixStream, request: &[u8], mut output: impl Write) -> Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.set_read_timeout(Some(Duration::from_secs(35)))?;
    let unknown = || "ownership outcome unknown; verify its state before retrying";
    stream.write_all(request).context(unknown())?;
    stream.shutdown(Shutdown::Write).context(unknown())?;
    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut response)
        .context(unknown())?;
    ensure!(response.len() as u64 <= MAX_RESPONSE_BYTES, "{}", unknown());
    let value: serde_json::Value =
        serde_json::from_slice(&response).map_err(|_| anyhow::anyhow!(unknown()))?;
    match value.get("status").and_then(serde_json::Value::as_str) {
        Some(
            "granted" | "queued" | "handoff_prepared" | "handoff_completed" | "handoff_pending"
            | "handoff_not_found",
        ) => {
            output
                .write_all(&response)
                .context("writing ownership response")?;
            output.flush()?;
            Ok(())
        }
        Some("invalid_request") => anyhow::bail!("invalid ownership request"),
        Some("unauthorized") => anyhow::bail!("ownership request authorization failed"),
        Some("unavailable") => anyhow::bail!("ownership service unavailable"),
        Some("rejected") => anyhow::bail!("ownership request rejected by the controller"),
        _ => Err(io::Error::other(unknown()).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_half_close_and_prints_success_receipt() {
        let (client, mut server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut request = Vec::new();
                server.read_to_end(&mut request).unwrap();
                assert_eq!(request, b"test request");
                server.write_all(b"{\"status\":\"queued\"}\n").unwrap();
            });
            let mut response = Vec::new();
            exchange(client, b"test request", &mut response).unwrap();
            assert_eq!(response, b"{\"status\":\"queued\"}\n");
        });
    }
}
