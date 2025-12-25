use str0m::{Output, net::Protocol};
use tokio::time::Instant;

pub struct AgentCore {
    rtc: str0m::Rtc,
}

impl AgentCore {
    pub fn new(rtc: str0m::Rtc) -> Self {
        Self { rtc }
    }

    fn poll(&mut self) -> Option<Instant> {
        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    return Some(deadline.into());
                }
                Ok(Output::Transmit(tx)) => match tx.proto {
                    Protocol::Udp => self.udp_batcher.push_back(tx.destination, &tx.contents),
                    Protocol::Tcp => self.tcp_batcher.push_back(tx.destination, &tx.contents),
                    _ => {}
                },
                Ok(Output::Event(event)) => self.handle_event(event),
                Err(e) => {
                    self.disconnect(e.into());
                    return None;
                }
            }
        }

        None
    }
}
