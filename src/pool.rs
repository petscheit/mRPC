use crate::worker::{EvmWorker, Priority};
use crate::{Error, RpcMessage, RpcRequest};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
pub struct WorkerPool {
    connections: HashMap<String, Vec<PoolConnection>>,
}

pub struct PoolConnection {
    id: u8,
    chain_id: String,
    url: String,
    priority: Priority,
    requests_per_second: u32,
    thread_count: u32,
    workers: Vec<WorkerInfo>,
}

impl PoolConnection {
    pub fn new(id: u8, chain_id: String, url: String, priority: Priority, requests_per_second: u32, thread_count: u32) -> Self {
        Self { 
            id, 
            chain_id, 
            url, 
            priority, 
            requests_per_second, 
            thread_count, 
            workers: vec![] 
        }
    }

    pub fn initialize_workers(&mut self) {
        self.workers = (1..=self.thread_count).map(|id| {
            let (tx, rx) = mpsc::channel(100);
            let worker = EvmWorker::new(
                id as u8,
                self.chain_id.clone(),
                self.url.clone(),
                self.priority.clone(),
                self.requests_per_second / self.thread_count,
            );

            let pending_requests = Arc::new(AtomicUsize::new(0));
            let pending_clone = pending_requests.clone();

            tokio::spawn(async move {
                worker.run(rx, pending_clone).await;
            });

            WorkerInfo {
                sender: tx,
                priority: self.priority.clone(),
                pending_requests,
            }
        }).collect();
    }

    async fn send_request(&self, request: RpcRequest) -> Result<Value, Error> {
        // Find the least loaded high-priority worker or fallback to least loaded any worker
        let worker_info = self.workers.iter()
            .min_by_key(|w| w.pending_requests.load(Ordering::Relaxed))
            .or_else(|| self.workers.iter()
                .min_by_key(|w| w.pending_requests.load(Ordering::Relaxed)))
            .ok_or_else(|| Error::CustomError("No workers available".to_string()))?;

        let (response_tx, response_rx) = oneshot::channel();
        let message = RpcMessage {
            request,
            response_tx,
        };

        worker_info.pending_requests.fetch_add(1, Ordering::Relaxed);
        
        worker_info.sender.send(message).await
            .map_err(|e| Error::CustomError(e.to_string()))?;

        response_rx.await
            .map_err(|e| Error::CustomError(e.to_string()))?
    }

    fn get_total_pending_requests(&self) -> usize {
        self.workers.iter()
            .map(|w| w.pending_requests.load(Ordering::Relaxed))
            .sum()
    }
}

impl WorkerPool {
    pub(crate) fn new() -> Self {
        let mut connections: HashMap<String, Vec<PoolConnection>> = HashMap::new();

        for mut connection in get_pool_connections() {
            connection.initialize_workers();
            connections
                .entry(connection.chain_id.clone())
                .or_default()
                .push(connection);
        }

        WorkerPool { connections }
    }

    pub(crate) async fn send_request(&self, request: RpcRequest) -> Result<Value, Error> {
        let connections = self.connections.get(&request.chain_id)
            .ok_or_else(|| Error::CustomError(format!("No connection for chain {}", request.chain_id)))?;

        // Select the connection with the least pending requests
        let connection = connections.iter()
            .min_by_key(|conn| conn.get_total_pending_requests())
            .ok_or_else(|| Error::CustomError("No connections available".to_string()))?;

        connection.send_request(request).await
    }
}

fn get_pool_connections() -> Vec<PoolConnection> {
    vec![
        PoolConnection { id: 1, chain_id: "11155111".to_string(), url: "https://sepolia.ethereum.iosis.tech".to_string(), priority: Priority::High, requests_per_second: 50, thread_count: 32, workers: vec![] },
    ]
}

struct WorkerInfo {
    sender: mpsc::Sender<RpcMessage>,
    priority: Priority,
    pending_requests: Arc<AtomicUsize>,
}