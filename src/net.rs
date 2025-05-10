#[cfg(test)]
pub use turmoil::net::UdpSocket;

#[cfg(not(test))]
pub use tokio::net::UdpSocket;
