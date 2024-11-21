use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, AtomicUsize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use crate::worker::{EvmWorker, Priority};
use crate::{Error, RpcMessage, RpcRequest};

pub struct WorkerPool {
    workers: HashMap<String, Vec<WorkerInfo>>,
}

struct WorkerInfo {
    sender: mpsc::Sender<RpcMessage>,
    priority: Priority,
    pending_requests: Arc<AtomicUsize>,
}

impl WorkerPool {
    pub(crate) fn new() -> Self {
        let mut workers = HashMap::new();

        let pool_connections = get_pool_connections();

        // Create workers for each pool connection
        for connection in pool_connections {
            let mut worker_threads: Vec<WorkerInfo> = (1..=connection.thread_count).map(|id| {
                let (tx, rx) = mpsc::channel(100);
                let worker = EvmWorker::new(
                    id as u8,
                    connection.chain_id.clone(),
                    connection.url.clone(),
                    connection.priority.clone(),
                    connection.requests_per_second / connection.thread_count,
                );

                // Clone the status before moving worker
                let pending_requests = Arc::new(AtomicUsize::new(0));
                let pending_clone = pending_requests.clone();

                // Spawn the worker task
                tokio::spawn(async move {
                    worker.run(rx, pending_clone).await;
                });

                WorkerInfo {
                    sender: tx,
                    priority: connection.priority,
                    pending_requests,
                }
            }).collect();

            workers.insert(connection.chain_id.clone(), worker_threads);
        }

        WorkerPool { workers }
    }

    pub(crate) async fn send_request(&self, request: RpcRequest) -> Result<Value, Error> {
        let workers = self.workers.get(&request.chain_id)
            .ok_or_else(|| Error::CustomError(format!("No workers for chain {}", request.chain_id)))?;

        // Create the message
        let (response_tx, response_rx) = oneshot::channel();
        let message = RpcMessage {
            request,
            response_tx,
        };

        // Find the least loaded high-priority worker or fallback to least loaded any worker
        let worker_info = workers.iter()
            .filter(|w| w.priority == Priority::High)
            .min_by_key(|w| w.pending_requests.load(Ordering::Relaxed))
            .or_else(|| workers.iter()
                .min_by_key(|w| w.pending_requests.load(Ordering::Relaxed)))
            .ok_or_else(|| Error::CustomError("No workers available".to_string()))?;

        // Increment the pending requests counter
        worker_info.pending_requests.fetch_add(1, Ordering::Relaxed);
        
        // Send to worker's channel (will be buffered if worker is busy)
        worker_info.sender.send(message).await
            .map_err(|e| Error::CustomError(e.to_string()))?;

        // Wait for and return the response
        response_rx.await
            .map_err(|e| Error::CustomError(e.to_string()))?
    }
}

fn get_pool_connections() -> Vec<EvmPoolConnection> {
    vec![
        EvmPoolConnection { id: 1, chain_id: "11155111".to_string(), url: "https://sepolia.ethereum.iosis.tech".to_string(), priority: Priority::High, requests_per_second: 50, thread_count: 32 },
    ]
}

pub struct EvmPoolConnection {
    id: u8,
    chain_id: String,
    url: String,
    priority: Priority,
    requests_per_second: u32,
    thread_count: u32,
}