use std::sync::Arc;
use std::sync::atomic::{Ordering, AtomicUsize};
use serde_json::Value;
use alloy::network::Ethereum;
use alloy::providers::{Provider, RootProvider};

use alloy::transports::http::{Client, Http};
use alloy::rpc::client::RpcClient;
use tokio::sync::mpsc;
use crate::{Error, RpcMessage, RpcRequest};
use tokio::time::{interval, Duration};

pub(crate) struct EvmWorker {
    id: u8,
    chain_id: String,
    priority: Priority,
    provider: RootProvider<Http<Client>, Ethereum>,
    requests_per_second: u32,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Priority {
    High,
    Low
}

impl EvmWorker {
    pub fn new(
        id: u8, 
        chain_id: String, 
        url: String, 
        priority: Priority, 
        requests_per_second: u32,
    ) -> Self {
        let provider = RootProvider::new(RpcClient::new_http(url.parse().unwrap()));
        EvmWorker { 
            id, 
            chain_id, 
            priority, 
            provider,
            requests_per_second,
        }
    }

    pub async fn process_request(&self, request: RpcRequest) -> Result<Value, Error> {
        const MAX_RETRIES: u32 = 3;
        const INITIAL_BACKOFF_MS: u64 = 100;

        let mut attempt = 0;
        loop {
            match self.provider
                .client()
                .request::<_, Value>(request.method.clone(), request.params.clone())
                .await
            {
                Ok(response) => return Ok(response),
                Err(e) => {
                    match e.as_error_resp() {
                        Some(err) => {
                            if err.code == 429 {
                                return Err(Error::RateLimitExceeded);
                            }

                            attempt += 1;
                            if attempt >= MAX_RETRIES {
                                return Err(Error::RemoteRpcError(e.into()));
                            }
                        }
                        // These errors could be transport error, decoding errors, etc. Shouldnt happen all that much
                        _ => return Err(Error::RemoteRpcError(e.into()))
                    }

                    // Exponential backoff: 100ms, 200ms, 400ms
                    let backoff = Duration::from_millis(INITIAL_BACKOFF_MS * (2_u64.pow(attempt - 1)));
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    pub async fn run(&self, mut rx: mpsc::Receiver<RpcMessage>, pending: Arc<AtomicUsize>) {
        let mut interval = interval(Duration::from_secs_f32(1.0 / self.requests_per_second as f32));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        while let Some(message) = rx.recv().await {
            interval.tick().await;
            
            let response = self.process_request(message.request).await;
            let _ = message.response_tx.send(response);
            pending.fetch_sub(1, Ordering::Relaxed);
        }
    }
}


