use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use noth_core::traits::ConnectionState;
use noth_core::types::UserId;

pub(crate) type Names = Arc<RwLock<HashMap<UserId, String>>>;

/// ConnectionState を EventHandler と Channel の間で共有するための AtomicU8 表現
#[derive(Clone)]
pub(crate) struct SharedState(Arc<AtomicU8>);

impl SharedState {
    pub(crate) fn new() -> Self {
        let s = Self(Arc::new(AtomicU8::new(0)));
        s.set(ConnectionState::Connecting);
        s
    }

    pub(crate) fn set(&self, state: ConnectionState) {
        self.0.store(state as u8, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> ConnectionState {
        match self.0.load(Ordering::Relaxed) {
            x if x == ConnectionState::Connected as u8 => ConnectionState::Connected,
            x if x == ConnectionState::Disconnected as u8 => ConnectionState::Disconnected,
            _ => ConnectionState::Connecting,
        }
    }
}

pub(crate) fn from_stage(stage: serenity::gateway::ConnectionStage) -> ConnectionState {
    use serenity::gateway::ConnectionStage as S;
    match stage {
        S::Connected => ConnectionState::Connected,
        S::Disconnected => ConnectionState::Disconnected,
        _ => ConnectionState::Connecting,
    }
}
