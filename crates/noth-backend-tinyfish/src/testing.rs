//! 各 tool crate のテストが共有する最小の HTTP モックサーバ。本番コードからは使わない
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{Endpoints, TinyFish};

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// クエリ文字列を含むパス
    pub target: String,
    /// 小文字化したヘッダ名と値
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

pub struct MockServer {
    pub base_url: String,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// `handler` には受信したリクエストと、これまでの受信数(0 始まり)を渡す
    pub async fn start<F>(handler: F) -> Self
    where
        F: Fn(&Recorded, usize) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let log = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let Some(req) = read_request(&mut sock).await else {
                    continue;
                };
                let n = {
                    let mut g = log.lock().expect("lock");
                    g.push(req.clone());
                    g.len() - 1
                };
                let reply = handler(&req, n);
                let mut out = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (k, v) in &reply.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&reply.body);
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        Self { base_url, requests }
    }

    pub fn backend(&self) -> TinyFish {
        let e = Endpoints {
            search: self.base_url.clone(),
            fetch: self.base_url.clone(),
            agent: self.base_url.clone(),
        };
        TinyFish::new("test-key".into(), std::time::Duration::from_secs(5), e).expect("client")
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().expect("lock").clone()
    }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buf.len() < body_start + len {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[body_start..]).to_string();
    Some(Recorded {
        method,
        target,
        headers,
        body,
    })
}

/// 一時ディレクトリに config.toml と .env を書いて読み込む
pub fn load_config(config: &str, env: &str) -> noth_core::config::Config {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "noth-tinyfish-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("config.toml"), config).expect("write config");
    std::fs::write(dir.join(".env"), env).expect("write env");
    let paths = noth_core::config::Paths::resolve(Some(&dir.join("config.toml"))).expect("paths");
    let loaded = noth_core::config::Config::load(paths);
    let _ = std::fs::remove_dir_all(&dir);
    loaded.expect("config")
}
