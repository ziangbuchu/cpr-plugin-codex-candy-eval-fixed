//! Loopback-only synthetic Responses server. No real key, account or model is used.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
    time::{timeout, Duration},
};

pub struct LocalModel {
    pub base: String,
    pub release: Arc<Semaphore>,
    started: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl LocalModel {
    pub async fn new() -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let release = Arc::new(Semaphore::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let permits = release.clone();
        let observed = started.clone();
        let task = tokio::spawn(async move {
            let mut children = JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let (mut socket, _) = connection.unwrap();
                        let release = permits.clone();
                        let started = observed.clone();
                        children.spawn(async move {
                            let mut bytes = Vec::new();
                            let mut buffer = [0u8; 4096];
                            let body_start = loop {
                                let n = socket.read(&mut buffer).await.unwrap();
                                assert!(n > 0 && bytes.len() < 1024 * 1024);
                                bytes.extend_from_slice(&buffer[..n]);
                                if let Some(offset) = bytes.windows(4).position(|v| v == b"\r\n\r\n") { break offset + 4; }
                            };
                            let headers = String::from_utf8_lossy(&bytes[..body_start]).to_ascii_lowercase();
                            assert!(headers.starts_with("post /v1/responses http/1.1"));
                            assert!(headers.contains("authorization: bearer fixture-only-not-a-real-key\r\n"));
                            let length: usize = headers.lines().find_map(|line|line.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                            while bytes.len() - body_start < length {
                                let n = socket.read(&mut buffer).await.unwrap();
                                assert!(n > 0);
                                bytes.extend_from_slice(&buffer[..n]);
                            }
                            let request: serde_json::Value = serde_json::from_slice(&bytes[body_start..body_start+length]).unwrap();
                            assert_eq!(request["model"], "gpt-6.1-sol");
                            started.fetch_add(1, Ordering::SeqCst);
                            release.acquire().await.unwrap().forget();
                            let numbers = (1..=355).cycle().take(4096).map(|n|n.to_string()).collect::<Vec<_>>().join(",");
                            let event = serde_json::json!({"type":"response.completed","response":{
                                "id":"fixture-response","status":"completed","model":"gpt-6.1-sol",
                                "output":[{"type":"message","content":[{"type":"output_text","text":numbers}]}],
                                "usage":{"input_tokens":12,"output_tokens":4096}
                            }});
                            let body = format!("event: response.completed\ndata: {event}\n\n");
                            let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len());
                            let _ = socket.write_all(response.as_bytes()).await;
                        });
                    }
                    done = children.join_next(), if !children.is_empty() => {
                        done.unwrap().expect("synthetic HTTP request failed");
                    }
                }
            }
        });
        Self {
            base,
            release,
            started,
            task,
        }
    }
    pub fn request_count(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
    pub async fn wait_started(&self, expected: usize) {
        timeout(Duration::from_secs(5), async {
            while self.started.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic HTTP execution did not start");
    }
}
impl Drop for LocalModel {
    fn drop(&mut self) {
        self.task.abort();
    }
}
