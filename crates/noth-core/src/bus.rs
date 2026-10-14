use tokio::sync::mpsc;

use crate::types::IncomingMessage;

#[derive(Debug, Clone)]
pub enum BusEvent {
    MessageReceived(IncomingMessage),
    /// MVP では worker が無いので流れない
    WorkerFinished {
        run_id: String,
    },
    IdleDetected,
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
