use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

use super::common::emit;
use crate::transport::OutChannel;
use tauqe_protocol::{
    config::{ChatGptSignInId, ChatGptSignInPrompt},
    events,
};
use tokio::sync::oneshot;

/// Outstanding invitations belong to this stdio connection.
pub struct SignIn {
    out: OutChannel,
    next: AtomicU64,
    pending: Mutex<HashMap<String, oneshot::Sender<()>>>,
}
impl SignIn {
    pub fn new(out: OutChannel) -> Self {
        Self {
            out,
            next: AtomicU64::new(1),
            pending: Default::default(),
        }
    }
    pub fn dismiss(&self, id: &str) {
        if let Some(waiter) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
        {
            let _ = waiter.send(());
        }
    }
}

struct Invitation<'a> {
    owner: &'a SignIn,
    id: String,
}
impl Drop for Invitation<'_> {
    fn drop(&mut self) {
        self.owner.dismiss(&self.id);
        emit(
            &self.owner.out,
            events::CHATGPT_SIGN_IN_CLOSED,
            serde_json::to_value(ChatGptSignInId {
                id: self.id.clone(),
            })
            .ok(),
        );
    }
}

#[async_trait::async_trait]
impl tauqe_flow::api::types::ChatGptSignInUi for SignIn {
    async fn wait_for_dismissal(&self, model: &str, authorization_url: &str) {
        let invitation = Invitation {
            owner: self,
            id: self.next.fetch_add(1, Ordering::Relaxed).to_string(),
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(invitation.id.clone(), tx);
        emit(
            &self.out,
            events::CHATGPT_SIGN_IN_PROMPT,
            serde_json::to_value(ChatGptSignInPrompt {
                id: invitation.id.clone(),
                model: model.into(),
                authorization_url: authorization_url.into(),
            })
            .ok(),
        );
        tokio::select! {_=rx=>(),_=self.out.sink_lost()=>()}
    }
}
