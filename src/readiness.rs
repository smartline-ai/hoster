use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep, timeout};

use crate::engine::ReadinessChecker;

/// Polls until a service answers or a deadline passes. With a health path it
/// does an HTTP GET (any status < 500 counts as ready); without one it settles
/// for a successful TCP connect.
pub struct NetworkReadiness {
    pub deadline: Duration,
    pub interval: Duration,
}

impl Default for NetworkReadiness {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(30),
            interval: Duration::from_millis(500),
        }
    }
}

#[async_trait]
impl ReadinessChecker for NetworkReadiness {
    async fn ready(&self, ip: &str, port: u16, health: Option<&str>) -> bool {
        let start = Instant::now();
        let addr = format!("{ip}:{port}");
        while start.elapsed() < self.deadline {
            let ok = match health {
                Some(path) => http_ok(&addr, path).await,
                None => TcpStream::connect(&addr).await.is_ok(),
            };
            if ok {
                return true;
            }
            sleep(self.interval).await;
        }
        false
    }
}

async fn http_ok(addr: &str, path: &str) -> bool {
    // Minimal HTTP/1.0 GET; ready when the status line is < 500.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(Ok(mut stream)) = timeout(Duration::from_secs(2), TcpStream::connect(addr)).await else {
        return false;
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let Ok(Ok(n)) = timeout(Duration::from_secs(2), stream.read(&mut buf)).await else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    // "HTTP/1.x NNN"
    head.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .map(|c| c < 500)
        .unwrap_or(false)
}
