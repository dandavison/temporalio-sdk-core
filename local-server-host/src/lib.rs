//! Hosts the local-server wasm module (built from the wasmpoc/ tree of the temporal branch
//! dandavison/temporalio-temporal:local-workflow-progress) as a gRPC service for sdk-core's
//! `ConnectionOptions::service_override`.
//!
//! The module never blocks and has no clock. The host gives it the time before every call, and
//! implements long polls: a poll that finds nothing waits until another call changes the module's
//! state, until the next task deadline the module reports, or until the poll's gRPC timeout.
use anyhow::{Result, bail};
use futures_util::FutureExt;
use prost::Message;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use temporalio_client::callback_based::{
    CallbackBasedGrpcService, GrpcRequest, GrpcSuccessResponse,
};
use temporalio_protos::temporal::api::workflowservice::v1::{
    GetWorkflowExecutionHistoryRequest, GetWorkflowExecutionHistoryResponse,
    PollActivityTaskQueueResponse, PollWorkflowTaskQueueResponse,
};
use tokio::{sync::watch, time::Instant};
use tonic::{Code, Status};
use wasmtime::{Engine, Instance, Linker, Module, Store, TypedFunc};
use wasmtime_wasi::{WasiCtxBuilder, p1::WasiP1Ctx};

/// Used when a long poll carries no `grpc-timeout`.
const DEFAULT_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(60);

/// Returns a gRPC service that serves every call from the module at `path` (a `.cwasm`).
pub fn grpc_service(path: &Path) -> Result<CallbackBasedGrpcService> {
    let server = Arc::new(LocalServer::load(path)?);
    Ok(CallbackBasedGrpcService {
        callback: Arc::new(move |request| {
            let server = server.clone();
            async move { server.serve(request).await }.boxed()
        }),
    })
}

pub struct LocalServer {
    guest: Mutex<Guest>,
    /// Incremented whenever a call may have made a task available.
    changes: watch::Sender<u64>,
}

impl LocalServer {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            guest: Mutex::new(Guest::new(path, now_nanos())?),
            changes: watch::Sender::new(0),
        })
    }

    async fn serve(&self, request: GrpcRequest) -> Result<GrpcSuccessResponse, Status> {
        let proto = if is_long_poll(&request) {
            self.long_poll(&request).await?
        } else {
            let (response, _) = self.call(&request.rpc, &request.proto)?;
            self.changes.send_modify(|n| *n += 1);
            response
        };
        Ok(GrpcSuccessResponse {
            headers: Default::default(),
            proto,
        })
    }

    async fn long_poll(&self, request: &GrpcRequest) -> Result<Vec<u8>, Status> {
        let timeout = Instant::now() + grpc_timeout(request).unwrap_or(DEFAULT_LONG_POLL_TIMEOUT);
        let mut changes = self.changes.subscribe();
        loop {
            changes.mark_unchanged();
            let (response, next_deadline) = self.call(&request.rpc, &request.proto)?;
            if !is_empty_long_poll_response(&request.rpc, &response)? {
                self.changes.send_modify(|n| *n += 1);
                return Ok(response);
            }
            let wake = next_deadline.map_or(timeout, |d| d.min(timeout));
            tokio::select! {
                _ = changes.changed() => {}
                _ = tokio::time::sleep_until(wake) => {
                    if wake == timeout {
                        return Ok(response);
                    }
                    // A task fell due; the next call runs it, which may unblock other polls.
                    self.changes.send_modify(|n| *n += 1);
                }
            }
        }
    }

    /// Calls the module at the current time and returns the response and the next task deadline.
    fn call(&self, rpc: &str, request: &[u8]) -> Result<(Vec<u8>, Option<Instant>), Status> {
        let mut guest = self.guest.lock().unwrap();
        guest.advance_time(now_nanos()).map_err(internal)?;
        let response = guest.call(rpc, request).map_err(internal)??;
        let next_deadline = guest.advance_time(now_nanos()).map_err(internal)?;
        Ok((response, next_deadline.map(instant_at)))
    }
}

fn is_long_poll(request: &GrpcRequest) -> bool {
    match request.rpc.as_str() {
        "PollWorkflowTaskQueue" | "PollActivityTaskQueue" | "PollNexusTaskQueue" => true,
        "GetWorkflowExecutionHistory" => {
            GetWorkflowExecutionHistoryRequest::decode(request.proto.as_ref())
                .is_ok_and(|r| r.wait_new_event)
        }
        _ => false,
    }
}

fn is_empty_long_poll_response(rpc: &str, response: &[u8]) -> Result<bool, Status> {
    let decode_error = |e: prost::DecodeError| internal(e.into());
    Ok(match rpc {
        "PollWorkflowTaskQueue" => PollWorkflowTaskQueueResponse::decode(response)
            .map_err(decode_error)?
            .task_token
            .is_empty(),
        "PollActivityTaskQueue" => PollActivityTaskQueueResponse::decode(response)
            .map_err(decode_error)?
            .task_token
            .is_empty(),
        "GetWorkflowExecutionHistory" => GetWorkflowExecutionHistoryResponse::decode(response)
            .map_err(decode_error)?
            .history
            .is_none_or(|h| h.events.is_empty()),
        _ => true,
    })
}

/// Parses the `grpc-timeout` header: an integer followed by a unit (H, M, S, m, u or n).
fn grpc_timeout(request: &GrpcRequest) -> Option<Duration> {
    let value = request.headers.get("grpc-timeout")?.to_str().ok()?;
    let (amount, unit) = value.split_at(value.len().checked_sub(1)?);
    let amount: u64 = amount.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(amount * 3600),
        "M" => Duration::from_secs(amount * 60),
        "S" => Duration::from_secs(amount),
        "m" => Duration::from_millis(amount),
        "u" => Duration::from_micros(amount),
        "n" => Duration::from_nanos(amount),
        _ => return None,
    })
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

fn instant_at(unix_nanos: i64) -> Instant {
    let from_now = Duration::from_nanos(unix_nanos.saturating_sub(now_nanos()).max(0) as u64);
    Instant::now() + from_now
}

fn internal(e: anyhow::Error) -> Status {
    Status::internal(e.to_string())
}

/// An instance of the module.
pub struct Guest {
    store: Store<WasiP1Ctx>,
    memory: wasmtime::Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<u32, ()>,
    call: TypedFunc<(u32, u32, u32, u32), u64>,
    advance_time: TypedFunc<i64, u64>,
}

impl Guest {
    pub fn new(path: &Path, now_nanos: i64) -> Result<Self> {
        let engine = Engine::default();
        let module = unsafe { Module::deserialize_file(&engine, path)? };
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |cx| cx)?;
        let mut store = Store::new(&engine, WasiCtxBuilder::new().inherit_stdio().build_p1());
        let instance: Instance = linker.instantiate(&mut store, &module)?;
        instance
            .get_typed_func::<(), ()>(&mut store, "_initialize")?
            .call(&mut store, ())?;
        let init = instance.get_typed_func::<i64, u32>(&mut store, "temporal_init")?;
        if init.call(&mut store, now_nanos)? != 0 {
            bail!("temporal_init failed");
        }
        Ok(Self {
            memory: instance.get_memory(&mut store, "memory").unwrap(),
            alloc: instance.get_typed_func(&mut store, "temporal_alloc")?,
            free: instance.get_typed_func(&mut store, "temporal_free")?,
            call: instance.get_typed_func(&mut store, "temporal_call")?,
            advance_time: instance.get_typed_func(&mut store, "temporal_advance_time")?,
            store,
        })
    }

    /// Calls a gRPC method of the module. The outer error is a failure of the module itself; the
    /// inner one is the gRPC status the method returned.
    pub fn call(&mut self, method: &str, request: &[u8]) -> Result<Result<Vec<u8>, Status>> {
        let method_ptr = self.write(method.as_bytes())?;
        let request_ptr = self.write(request)?;
        let packed = self.call.call(
            &mut self.store,
            (
                method_ptr,
                method.len() as u32,
                request_ptr,
                request.len() as u32,
            ),
        )?;
        self.free.call(&mut self.store, method_ptr)?;
        self.free.call(&mut self.store, request_ptr)?;
        self.read_response(packed)
    }

    /// Sets the module's clock, runs the tasks that are due, and returns the time (Unix
    /// nanoseconds) at which the next task is due.
    pub fn advance_time(&mut self, now_nanos: i64) -> Result<Option<i64>> {
        let packed = self.advance_time.call(&mut self.store, now_nanos)?;
        let deadline = prost_types::Timestamp::decode(self.read_response(packed)??.as_slice())?;
        Ok((deadline != prost_types::Timestamp::default())
            .then(|| deadline.seconds * 1_000_000_000 + i64::from(deadline.nanos)))
    }

    fn write(&mut self, bytes: &[u8]) -> Result<u32> {
        let ptr = self.alloc.call(&mut self.store, bytes.len() as u32)?;
        self.memory.write(&mut self.store, ptr as usize, bytes)?;
        Ok(ptr)
    }

    fn read_response(&mut self, packed: u64) -> Result<Result<Vec<u8>, Status>> {
        let (ptr, len) = ((packed >> 32) as u32, (packed & 0xffff_ffff) as usize);
        let mut out = vec![0u8; len];
        self.memory.read(&self.store, ptr as usize, &mut out)?;
        self.free.call(&mut self.store, ptr)?;
        Ok(match out[0] {
            0 => Ok(out[1..].to_vec()),
            code => Err(Status::new(
                Code::from_i32(code.into()),
                String::from_utf8_lossy(&out[1..]),
            )),
        })
    }
}
