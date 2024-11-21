use std::cmp::PartialEq;
use axum::{routing::{post}, Router, Json, http::StatusCode, ServiceExt};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::oneshot;

mod worker;
mod pool;

use alloy::providers::{Provider};
use alloy::transports::{RpcError, TransportError};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use crate::pool::WorkerPool;

struct ManyRpc {
    worker_pool: Arc<WorkerPool>,
}

impl ManyRpc {
    fn new() -> Self {
        let worker_pool = Arc::new(WorkerPool::new());
        ManyRpc { worker_pool }
    }

    fn init_routes(worker_pool: Arc<WorkerPool>) -> Router {
        let app = Router::new()
            .route("/evm", post(handle_evm))
            .route("/evm/:chain_id", post(|state, path, payload| 
                handle_evm_with_chain(state, path, payload))
            )
            .with_state(worker_pool);

        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3000));
        println!("Server running on http://{}", addr);
        app
    }
}

#[derive(Debug)]
pub enum Error {
    CustomError(String),
    RemoteRpcError(RpcError<TransportError>),
}

#[derive(Debug, Deserialize, Clone)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub method: String,
    pub params: Vec<Value>,
    pub id: Value,
    pub chain_id: String
}

// Add this new struct to handle request-response pairs
#[derive(Debug)]
struct RpcMessage {
    request: RpcRequest,
    response_tx: oneshot::Sender<Result<Value, Error>>,
}

// Handler function
async fn handle_evm(
    State(pool): State<Arc<WorkerPool>>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    // Parse the incoming JSON into our RpcRequest structure
    let request = match serde_json::from_value::<RpcRequest>(payload) {
        Ok(req) => req,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(format!("Invalid request format: {}", e))
                .unwrap()
                .into_response();
        }
    };

    match pool.send_request(request).await {
        Ok(response) => {
            println!("Worker returned result");
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(response.to_string())
                .unwrap()
                .into_response()
        }
        Err(e) => {
            let error_message = format!("Error processing request: {:?}", e);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(error_message)
                .unwrap()
                .into_response()
        }
    }
}

// Add new handler function that reuses handle_evm
async fn handle_evm_with_chain(
    state: State<Arc<WorkerPool>>,
    Path(chain_id): Path<String>,
    Json(mut payload): Json<Value>,
) -> impl IntoResponse {
    if let Value::Object(ref mut map) = payload {
        map.insert("chain_id".to_string(), Value::String(chain_id));
    }
    handle_evm(state, Json(payload)).await
}

// Add main function at the end
#[tokio::main]
async fn main() {
    let worker_pool = Arc::new(WorkerPool::new());
    let app = ManyRpc::init_routes(worker_pool);
    
    axum::Server::bind(&"127.0.0.1:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}