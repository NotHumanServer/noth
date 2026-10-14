use tokio::sync::mpsc;

use crate::types::IncomingMessage;

/// Channel から Core への入力。Worker の完了とアイドル検知は actor 自身が仕事のキュー(`actor::Job`)に直接積むので、ここには流さない
#[derive(Debug, Clone)]
pub enum BusEvent {
    MessageReceived(IncomingMessage),
}

#[derive(Clone)]
pub struct BusHandle {
    tx: mpsc::Sender<BusEvent>,
}

impl BusHandle {
    pub fn new(tx: mpsc::Sender<BusEvent>) -> Self {
        Self { tx }
    }

    pub async fn publish(&self, event: BusEvent) -> crate::Result<()> {
        self.tx
            .send(event)
            .await
            .map_err(|_| crate::Error::Other("event bus closed".into()))
    }
}

pub fn channel(capacity: usize) -> (BusHandle, mpsc::Receiver<BusEvent>) {
    let (tx, rx) = mpsc::channel(capacity);
    (BusHandle::new(tx), rx)
}
