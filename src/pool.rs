use crate::worker::{EvmWorker, Priority};
use crate::{Error, RpcMessage, RpcRequest};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use std::sync::RwLock;

pub struct WorkerPool {
    connections: HashMap<String, Vec<PoolConnection>>,
}

#[derive(Clone)]
pub struct PoolConnection {
    id: u8,
    chain_id: String,
    url: String,
    priority: Priority,
    requests_per_second: u32,
    thread_count: u32,
    workers: Vec<WorkerInfo>,
    rate_limit_info: Arc<RwLock<RateLimitInfo>>,
}

struct RateLimitInfo {
    is_rate_limited: bool,
    timestamp: u64,
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
            workers: vec![],
            rate_limit_info: Arc::new(RwLock::new(RateLimitInfo { is_rate_limited: false, timestamp: 0 })),
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

        match response_rx.await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(Error::RateLimitExceeded)) => {
                // Update the PoolConnection to rate limit mode
                if let Ok(mut rate_limit) = self.rate_limit_info.write() {
                    rate_limit.is_rate_limited = true;
                    rate_limit.timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    
                    // Spawn the checker only when we first detect rate limiting
                    self.spawn_rate_limit_checker();
                }
                Err(Error::RateLimitExceeded)
            },
            Ok(Err(e)) => Err(e),
            Err(e) => Err(Error::CustomError(e.to_string())),
        }
    }

    fn get_total_pending_requests(&self) -> usize {
        self.workers.iter()
            .map(|w| w.pending_requests.load(Ordering::Relaxed))
            .sum()
    }
    
    async fn check_rate_limit_status(&self) -> bool {
        // Create a basic eth_blockNumber request
        let request = RpcRequest {
            chain_id: self.chain_id.clone(),
            method: "eth_chainId".to_string(),
            params: vec![],  // empty params array
            jsonrpc: "2.0".to_string(),
            id: 1.into(),
        };

        // Try to send the request
        match self.send_request(request).await {
            Ok(_) => {
                // If successful, remove rate limit
                if let Ok(mut rate_limit) = self.rate_limit_info.write() {
                    println!("Rate limit check successful - removing rate limit status");
                    rate_limit.is_rate_limited = false;
                    rate_limit.timestamp = 0;
                }
                true
            }
            Err(_) => false,
        }
    }

    fn spawn_rate_limit_checker(&self) {
        let rate_limit_info = self.rate_limit_info.clone();
        let connection = self.clone();

        println!("Spawning rate limit checker task");
        tokio::spawn(async move {
            // Initial delay before first check
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            
            while rate_limit_info.read().map(|info| info.is_rate_limited).unwrap_or(false) {
                // Try to check if rate limit is still active
                connection.check_rate_limit_status().await;
                
                // If we're no longer rate limited, exit the loop
                if !rate_limit_info.read().map(|info| info.is_rate_limited).unwrap_or(false) {
                    println!("Rate limit removed - terminating checker task");
                    break;
                }
                
                // Sleep before next check
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
            println!("Rate limit checker task terminated");
        });
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
        let connections = self.connections
            .get(&request.chain_id)
            .ok_or_else(|| Error::CustomError(format!("No connection for chain {}", request.chain_id)))?;

        // Find the appropriate connection and send the request
        let connection = connections
            .iter()
            .filter(|conn| {
                conn.rate_limit_info
                    .read()
                    .map(|info| !info.is_rate_limited)
                    .unwrap_or(true)
            })
            .min_by_key(|conn| conn.get_total_pending_requests())
            .ok_or_else(|| Error::CustomError("No connections available".to_string()))?;

        connection.send_request(request).await
    }

}

fn get_pool_connections() -> Vec<PoolConnection> {
    vec![
        PoolConnection::new(1, "11155111".to_string(), "https://sepolia.infura.io/v3/66dda5ed7d56432a82c8da4ac54fde8e".to_string(), Priority::High, 5, 5),
        PoolConnection::new(2, "1".to_string(), "https://mainnet.infura.io/v3/66dda5ed7d56432a82c8da4ac54fde8e".to_string(), Priority::High, 5, 5),
    ]
}

#[derive(Clone)]
struct WorkerInfo {
    sender: mpsc::Sender<RpcMessage>,
    priority: Priority,
    pending_requests: Arc<AtomicUsize>,
}
