// Each integration test binary compiles this module separately and uses only
// part of it, so unused-code warnings here are structural, not real.
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// What the stub upstream saw on its most recent request.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub host: Option<String>,
    pub path: Option<String>,
    pub forwarded_host: Option<String>,
    pub forwarded_proto: Option<String>,
}

pub struct Upstream {
    pub addr: SocketAddr,
    pub seen: Arc<Mutex<Seen>>,
}

/// Spawns an upstream that replies `200 OK` with `body` and records the
/// request it received.
pub async fn spawn_upstream(body: &'static str) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let seen_for_task = seen.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let seen = seen_for_task.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let seen = seen.clone();
                    async move {
                        let header = |name: &str| {
                            req.headers()
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string)
                        };
                        *seen.lock().unwrap() = Seen {
                            host: header("host"),
                            path: Some(req.uri().path().to_string()),
                            forwarded_host: header("x-forwarded-host"),
                            forwarded_proto: header("x-forwarded-proto"),
                        };
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .body(Full::new(Bytes::from(body)))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });

    Upstream { addr, seen }
}
