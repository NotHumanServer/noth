use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

/// 各ストリームで先頭と末尾をそれぞれこの量だけ残し、間を捨てる。エラーは末尾に出ることが多い
const KEEP_HEAD_BYTES: usize = 32 * 1024;
const KEEP_TAIL_BYTES: usize = 32 * 1024;
/// sh が終了した後、背景プロセスが握っているパイプの EOF を待つ上限
const DRAIN_GRACE: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: usize,
}

impl Capture {
    fn push(&mut self, chunk: &[u8]) {
        let take = KEEP_HEAD_BYTES
            .saturating_sub(self.head.len())
            .min(chunk.len());
        self.head.extend_from_slice(&chunk[..take]);
        self.tail.extend_from_slice(&chunk[take..]);
        if self.tail.len() > KEEP_TAIL_BYTES {
            let excess = self.tail.len() - KEEP_TAIL_BYTES;
            self.tail.drain(..excess);
            self.omitted += excess;
        }
    }

    fn render(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.head).into_owned();
        if self.omitted > 0 {
            s.push_str(&format!("\n...(中略: {} バイト省略)...\n", self.omitted));
        }
        s.push_str(&String::from_utf8_lossy(&self.tail));
        s
    }
}

pub(crate) struct Reader {
    capture: Arc<Mutex<Capture>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) fn spawn_capture<R>(pipe: Option<R>) -> Reader
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let capture = Arc::new(Mutex::new(Capture::default()));
    let task = pipe.map(|mut pipe| {
        let capture = capture.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                capture.lock().unwrap().push(&buf[..n]);
            }
        })
    });
    Reader { capture, task }
}

impl Reader {
    /// setsid などでグループを抜けた子がパイプを握っていても、猶予を過ぎたら打ち切って集めた分を返す
    pub(crate) async fn finish(self) -> String {
        if let Some(mut task) = self.task
            && tokio::time::timeout(DRAIN_GRACE, &mut task).await.is_err()
        {
            task.abort();
        }
        self.capture.lock().unwrap().render()
    }
}
