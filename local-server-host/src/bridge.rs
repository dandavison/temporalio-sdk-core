//! Runs, on the local server, workflow runs that a server owns, using the server's local-execution
//! protocol (branch `sj/local-first-execution` of temporalio/temporal).
//!
//! For each task queue a worker polls on the local server, the host polls the server's task queue
//! with local-execution options. The server then hands over the run's pending workflow task
//! without starting it, together with a lease on the run. The host imports the run into the module,
//! where the worker runs it. At each sync interval the host sends the server the history the run
//! added and so renews the lease; when the run closes, it sends the rest of the history and
//! releases the lease. If the server refuses a sync, the lease is lost and the host deletes the run.
//!
//! Limitations:
//! - Only new runs can be imported: the module cannot rebuild a run from a longer history. Any
//!   other run is released back to the server, which will offer it again.
//! - Local progress does not pause when a sync fails, so a run keeps running locally after its
//!   lease expired, and that progress is discarded.
//! - Queries sent to an owned run fail. The protocol's relay of signals, updates, cancellation and
//!   queries to the owner is not implemented by the server.
//! - The connection to the server is plaintext, with no API key.
use crate::{LocalServer, internal};
use prost::Message;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use temporalio_protos::temporal::api::{
    common::v1::{DataBlob, WorkflowExecution},
    enums::v1::{EncodingType, EventType, QueryResultType, TaskQueueKind},
    history::v1::History,
    taskqueue::v1::TaskQueue,
    workflowservice::v1::{
        PollWorkflowTaskQueueRequest, PollWorkflowTaskQueueResponse,
        RespondQueryTaskCompletedRequest,
    },
};
use tonic::{
    Code, Status,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    transport::{Channel, Endpoint},
};

const PROTOCOL_VERSION: i32 = 1;
const WORKFLOW_SERVICE: &str = "/temporal.api.workflowservice.v1.WorkflowService/";
const SYNC_LOCAL_EXECUTION: &str =
    "/temporal.server.api.adminservice.v1.AdminService/SyncLocalExecution";
const POLL_TIMEOUT: Duration = Duration::from_secs(70);
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// The server that owns the runs a local server runs.
#[derive(Clone, Debug)]
pub struct Upstream {
    /// URL of the server's frontend, e.g. `http://localhost:7233`.
    pub target_url: String,
    /// How often the host sends the server a run's new history. The lease lasts three intervals.
    pub sync_interval: Duration,
}

pub(crate) struct Bridge {
    upstream: Upstream,
    server_id: String,
    channel: OnceLock<Channel>,
    /// The (namespace, task queue) pairs from which runs are acquired.
    task_queues: Mutex<HashSet<(String, String)>>,
    leases: Mutex<HashMap<Run, Lease>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Run {
    namespace: String,
    workflow_id: String,
    run_id: String,
}

#[derive(Clone)]
struct Lease {
    token: Vec<u8>,
    epoch: i64,
    /// The last event the server has.
    cursor: VersionHistoryItem,
    /// Number of syncs the server accepted, which makes sync IDs unique.
    syncs: u64,
    /// A sync whose outcome is unknown. It is resent unchanged, so that the server can recognize
    /// it if it was applied.
    pending: Option<SyncLocalExecutionRequest>,
}

impl Bridge {
    pub(crate) fn new(upstream: Upstream) -> Self {
        Self {
            upstream,
            server_id: format!("local-server-host-{}", std::process::id()),
            channel: OnceLock::new(),
            task_queues: Mutex::default(),
            leases: Mutex::default(),
        }
    }

    /// Starts acquiring runs from the task queue of a workflow task poll, and starts syncing when
    /// the first such poll arrives. Must be called from within a tokio runtime.
    pub(crate) fn observe_poll(self: &Arc<Self>, server: &Arc<LocalServer>, request: &[u8]) {
        let Ok(poll) = PollWorkflowTaskQueueRequest::decode(request) else {
            return;
        };
        let Some(task_queue) = poll.task_queue else {
            return;
        };
        if task_queue.kind == TaskQueueKind::Sticky as i32 {
            return;
        }
        let mut task_queues = self.task_queues.lock().unwrap();
        if task_queues.is_empty() {
            tokio::spawn(sync_runs(self.clone(), Arc::downgrade(server)));
        }
        if task_queues.insert((poll.namespace.clone(), task_queue.name.clone())) {
            tokio::spawn(acquire_runs(
                self.clone(),
                Arc::downgrade(server),
                poll.namespace,
                task_queue.name,
            ));
        }
    }

    /// Long-polls the server for a run to own. Returns `None` when the poll times out or delivered
    /// a query.
    async fn poll(
        &self,
        namespace: &str,
        task_queue: &str,
    ) -> Result<Option<(PollWorkflowTaskQueueResponse, LocalExecutionTaskInfo)>, Status> {
        let mut request = PollWorkflowTaskQueueRequest {
            namespace: namespace.to_owned(),
            task_queue: Some(TaskQueue {
                name: task_queue.to_owned(),
                kind: TaskQueueKind::Normal as i32,
                ..Default::default()
            }),
            identity: self.server_id.clone(),
            ..Default::default()
        }
        .encode_to_vec();
        // Concatenated protobuf messages merge: this sets the request's local-execution options.
        PollWorkflowTaskQueueRequestExtension {
            local_execution_options: Some(LocalExecutionPollOptions {
                local_server_id: self.server_id.clone(),
                protocol_version: PROTOCOL_VERSION,
                sync_interval: Some(duration(self.upstream.sync_interval)),
                requested_lease_duration: Some(duration(3 * self.upstream.sync_interval)),
            }),
        }
        .encode(&mut request)
        .map_err(|e| Status::internal(e.to_string()))?;
        let response = self
            .unary(
                &format!("{WORKFLOW_SERVICE}PollWorkflowTaskQueue"),
                request,
                POLL_TIMEOUT,
            )
            .await?;
        let task = decode::<PollWorkflowTaskQueueResponse>(&response)?;
        // An acquired run's workflow task has no task token: the server did not start it.
        let info =
            decode::<PollWorkflowTaskQueueResponseExtension>(&response)?.local_execution_info;
        match info {
            Some(info) => Ok(Some((task, info))),
            None if task.query.is_some() => {
                self.reject_query(namespace, task.task_token).await?;
                Ok(None)
            }
            None if task.task_token.is_empty() => Ok(None),
            None => Err(Status::internal(
                "the server returned a workflow task without local-execution ownership",
            )),
        }
    }

    /// Imports an acquired run into the module, or releases it if the module cannot run it.
    async fn adopt(
        &self,
        server: &LocalServer,
        namespace: String,
        task: PollWorkflowTaskQueueResponse,
        info: LocalExecutionTaskInfo,
    ) {
        let execution = task.workflow_execution.unwrap_or_default();
        let run = Run {
            namespace: namespace.clone(),
            workflow_id: execution.workflow_id.clone(),
            run_id: execution.run_id.clone(),
        };
        let cursor = VersionHistoryItem {
            event_id: info.last_synchronized_event_id,
            version: info.last_synchronized_event_version,
        };
        let lease = Lease {
            token: info.ownership_token,
            epoch: info.fencing_epoch,
            cursor: cursor.clone(),
            syncs: 0,
            pending: None,
        };
        let events = task.history.map(|h| h.events).unwrap_or_default();
        let import = ImportWorkflowExecutionRequest {
            namespace,
            execution: Some(execution),
            history_batches: vec![proto3_blob(History { events })],
            version_history: Some(VersionHistory {
                branch_token: vec![],
                items: vec![cursor.clone()],
            }),
        };
        let imported = if task.next_page_token.is_empty() {
            server
                .call(
                    "AdminService/ImportWorkflowExecution",
                    &import.encode_to_vec(),
                )
                .await
                .map(|_| ())
        } else {
            Err(Status::unimplemented(
                "the run's history has more than one page",
            ))
        };
        match imported {
            Ok(()) => {
                self.leases.lock().unwrap().insert(run, lease);
                server.notify_change();
            }
            Err(status) => {
                eprintln!("local server cannot run {run:?}, releasing it: {status}");
                let request = self.sync_request(&run, &lease, vec![], cursor, true);
                if let Err(status) = self.sync(request).await {
                    eprintln!("releasing {run:?} failed: {status}");
                }
            }
        }
    }

    /// Sends the server the history each owned run added: a run that closed at once, any other
    /// run when the sync interval elapsed.
    async fn sync_owned_runs(&self, server: &LocalServer, interval_elapsed: bool) {
        let leases = self.leases.lock().unwrap().clone();
        for (run, lease) in leases {
            if let Err(status) = self.sync_run(server, &run, lease, interval_elapsed).await {
                eprintln!("syncing {run:?} failed, will retry: {status}");
            }
        }
    }

    async fn sync_run(
        &self,
        server: &LocalServer,
        run: &Run,
        lease: Lease,
        interval_elapsed: bool,
    ) -> Result<(), Status> {
        let request = match lease.pending.clone() {
            Some(pending) => pending,
            None => {
                let tail = self.read_tail(server, run, &lease.cursor).await?;
                let closes = tail.history_batches.last().is_some_and(closes_run);
                if !closes && !interval_elapsed {
                    return Ok(());
                }
                let new_cursor = tail
                    .version_history
                    .as_ref()
                    .and_then(|h| h.items.last().cloned())
                    .ok_or_else(|| Status::internal("module returned no version history"))?;
                self.sync_request(run, &lease, tail.history_batches, new_cursor, closes)
            }
        };
        self.set_pending(run, Some(request.clone()));
        match self.sync(request.clone()).await {
            Ok(_) if request.release => self.forget(server, run).await,
            Ok(_) => {
                let mut leases = self.leases.lock().unwrap();
                if let Some(lease) = leases.get_mut(run) {
                    lease.cursor = VersionHistoryItem {
                        event_id: request.new_event_id,
                        version: request.new_event_version,
                    };
                    lease.syncs += 1;
                    lease.pending = None;
                }
                Ok(())
            }
            Err(status) if status.code() == Code::FailedPrecondition => {
                eprintln!("lost ownership of {run:?}: {}", status.message());
                self.forget(server, run).await
            }
            Err(status) => Err(status),
        }
    }

    fn sync_request(
        &self,
        run: &Run,
        lease: &Lease,
        history_batches: Vec<DataBlob>,
        new_cursor: VersionHistoryItem,
        release: bool,
    ) -> SyncLocalExecutionRequest {
        SyncLocalExecutionRequest {
            namespace: run.namespace.clone(),
            execution: Some(execution(run)),
            protocol_version: PROTOCOL_VERSION,
            local_server_id: self.server_id.clone(),
            sync_id: format!("{}/{}/{}", self.server_id, lease.epoch, lease.syncs),
            previous_event_id: lease.cursor.event_id,
            previous_event_version: lease.cursor.version,
            new_event_id: new_cursor.event_id,
            new_event_version: new_cursor.version,
            history_batches,
            version_history: Some(VersionHistory {
                branch_token: vec![],
                items: vec![new_cursor],
            }),
            release,
            ownership_token: lease.token.clone(),
            fencing_epoch: lease.epoch,
        }
    }

    async fn read_tail(
        &self,
        server: &LocalServer,
        run: &Run,
        cursor: &VersionHistoryItem,
    ) -> Result<GetWorkflowExecutionRawHistoryV2Response, Status> {
        let request = GetWorkflowExecutionRawHistoryV2Request {
            namespace_id: run.namespace.clone(),
            execution: Some(execution(run)),
            start_event_id: cursor.event_id,
            start_event_version: cursor.version,
        };
        let (response, _) = server
            .call(
                "AdminService/GetWorkflowExecutionRawHistoryV2",
                &request.encode_to_vec(),
            )
            .await?;
        decode(&response)
    }

    /// Stops owning a run and deletes it from the module.
    async fn forget(&self, server: &LocalServer, run: &Run) -> Result<(), Status> {
        self.leases.lock().unwrap().remove(run);
        let request = AdminDeleteWorkflowExecutionRequest {
            namespace: run.namespace.clone(),
            execution: Some(execution(run)),
        };
        server
            .call(
                "AdminService/DeleteWorkflowExecution",
                &request.encode_to_vec(),
            )
            .await?;
        server.notify_change();
        Ok(())
    }

    fn set_pending(&self, run: &Run, pending: Option<SyncLocalExecutionRequest>) {
        if let Some(lease) = self.leases.lock().unwrap().get_mut(run) {
            lease.pending = pending;
        }
    }

    async fn sync(
        &self,
        request: SyncLocalExecutionRequest,
    ) -> Result<SyncLocalExecutionResponse, Status> {
        let response = self
            .unary(SYNC_LOCAL_EXECUTION, request.encode_to_vec(), RPC_TIMEOUT)
            .await?;
        decode(&response)
    }

    async fn reject_query(&self, namespace: &str, task_token: Vec<u8>) -> Result<(), Status> {
        let request = RespondQueryTaskCompletedRequest {
            namespace: namespace.to_owned(),
            task_token,
            completed_type: QueryResultType::Failed as i32,
            error_message: "queries are not supported while a local server owns the workflow"
                .to_owned(),
            ..Default::default()
        };
        self.unary(
            &format!("{WORKFLOW_SERVICE}RespondQueryTaskCompleted"),
            request.encode_to_vec(),
            RPC_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    async fn unary(&self, path: &str, body: Vec<u8>, timeout: Duration) -> Result<Vec<u8>, Status> {
        let channel = self.channel()?.clone();
        let mut grpc = tonic::client::Grpc::new(channel);
        grpc.ready()
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let mut request = tonic::Request::new(body);
        request.set_timeout(timeout);
        let path = path
            .parse()
            .map_err(|_| Status::internal(format!("invalid path {path}")))?;
        Ok(grpc.unary(request, path, RawCodec).await?.into_inner())
    }

    fn channel(&self) -> Result<&Channel, Status> {
        if let Some(channel) = self.channel.get() {
            return Ok(channel);
        }
        let channel = Endpoint::from_shared(self.upstream.target_url.clone())
            .map_err(|e| Status::invalid_argument(e.to_string()))?
            .connect_lazy();
        Ok(self.channel.get_or_init(|| channel))
    }
}

async fn acquire_runs(
    bridge: Arc<Bridge>,
    server: Weak<LocalServer>,
    namespace: String,
    task_queue: String,
) {
    while server.strong_count() > 0 {
        match bridge.poll(&namespace, &task_queue).await {
            Ok(Some((task, info))) => {
                let Some(server) = server.upgrade() else {
                    return;
                };
                bridge.adopt(&server, namespace.clone(), task, info).await;
            }
            Ok(None) => {}
            Err(status) => {
                eprintln!("polling {task_queue} on the server failed: {status}");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

async fn sync_runs(bridge: Arc<Bridge>, server: Weak<LocalServer>) {
    let Some(mut changes) = server.upgrade().map(|s| s.changes.subscribe()) else {
        return;
    };
    let mut interval = tokio::time::interval(bridge.upstream.sync_interval);
    loop {
        let interval_elapsed = tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
                false
            }
            _ = interval.tick() => true,
        };
        let Some(server) = server.upgrade() else {
            return;
        };
        bridge.sync_owned_runs(&server, interval_elapsed).await;
    }
}

fn closes_run(batch: &DataBlob) -> bool {
    History::decode(batch.data.as_slice())
        .ok()
        .and_then(|h| h.events.last().map(|e| e.event_type))
        .and_then(|t| EventType::try_from(t).ok())
        .is_some_and(|t| {
            matches!(
                t,
                EventType::WorkflowExecutionCompleted
                    | EventType::WorkflowExecutionFailed
                    | EventType::WorkflowExecutionTimedOut
                    | EventType::WorkflowExecutionCanceled
                    | EventType::WorkflowExecutionTerminated
                    | EventType::WorkflowExecutionContinuedAsNew
            )
        })
}

fn execution(run: &Run) -> WorkflowExecution {
    WorkflowExecution {
        workflow_id: run.workflow_id.clone(),
        run_id: run.run_id.clone(),
    }
}

fn proto3_blob(history: History) -> DataBlob {
    DataBlob {
        encoding_type: EncodingType::Proto3 as i32,
        data: history.encode_to_vec(),
    }
}

fn duration(d: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}

fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M, Status> {
    M::decode(bytes).map_err(|e| internal(e.into()))
}

/// Passes encoded messages through, so that requests can carry fields the SDK's protos lack.
#[derive(Clone, Copy)]
struct RawCodec;

impl Codec for RawCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = RawCodec;
    type Decoder = RawCodec;

    fn encoder(&mut self) -> Self::Encoder {
        *self
    }

    fn decoder(&mut self) -> Self::Decoder {
        *self
    }
}

impl Encoder for RawCodec {
    type Item = Vec<u8>;
    type Error = Status;

    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        bytes::BufMut::put_slice(dst, &item);
        Ok(())
    }
}

impl Decoder for RawCodec {
    type Item = Vec<u8>;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, Status> {
        let mut out = vec![0; bytes::Buf::remaining(src)];
        bytes::Buf::copy_to_slice(src, &mut out);
        Ok(Some(out))
    }
}

// Messages of the local-execution protocol and of the server's AdminService, which the SDK's
// protos do not include. Field numbers are those of branch sj/local-first-execution.

#[derive(Clone, PartialEq, Message)]
struct PollWorkflowTaskQueueRequestExtension {
    #[prost(message, optional, tag = "11")]
    local_execution_options: Option<LocalExecutionPollOptions>,
}

#[derive(Clone, PartialEq, Message)]
struct LocalExecutionPollOptions {
    #[prost(string, tag = "1")]
    local_server_id: String,
    #[prost(int32, tag = "2")]
    protocol_version: i32,
    #[prost(message, optional, tag = "3")]
    sync_interval: Option<prost_types::Duration>,
    #[prost(message, optional, tag = "4")]
    requested_lease_duration: Option<prost_types::Duration>,
}

#[derive(Clone, PartialEq, Message)]
struct PollWorkflowTaskQueueResponseExtension {
    #[prost(message, optional, tag = "20")]
    local_execution_info: Option<LocalExecutionTaskInfo>,
}

#[derive(Clone, PartialEq, Message)]
struct LocalExecutionTaskInfo {
    #[prost(bytes = "vec", tag = "1")]
    ownership_token: Vec<u8>,
    #[prost(int64, tag = "2")]
    fencing_epoch: i64,
    #[prost(message, optional, tag = "3")]
    lease_expiration_time: Option<prost_types::Timestamp>,
    #[prost(int64, tag = "4")]
    last_synchronized_event_id: i64,
    #[prost(int64, tag = "5")]
    last_synchronized_event_version: i64,
}

#[derive(Clone, PartialEq, Message)]
struct VersionHistoryItem {
    #[prost(int64, tag = "1")]
    event_id: i64,
    #[prost(int64, tag = "2")]
    version: i64,
}

#[derive(Clone, PartialEq, Message)]
struct VersionHistory {
    #[prost(bytes = "vec", tag = "1")]
    branch_token: Vec<u8>,
    #[prost(message, repeated, tag = "2")]
    items: Vec<VersionHistoryItem>,
}

#[derive(Clone, PartialEq, Message)]
struct ImportWorkflowExecutionRequest {
    #[prost(string, tag = "1")]
    namespace: String,
    #[prost(message, optional, tag = "2")]
    execution: Option<WorkflowExecution>,
    #[prost(message, repeated, tag = "3")]
    history_batches: Vec<DataBlob>,
    #[prost(message, optional, tag = "4")]
    version_history: Option<VersionHistory>,
}

#[derive(Clone, PartialEq, Message)]
struct GetWorkflowExecutionRawHistoryV2Request {
    #[prost(string, tag = "9")]
    namespace_id: String,
    #[prost(message, optional, tag = "2")]
    execution: Option<WorkflowExecution>,
    #[prost(int64, tag = "3")]
    start_event_id: i64,
    #[prost(int64, tag = "4")]
    start_event_version: i64,
}

#[derive(Clone, PartialEq, Message)]
struct GetWorkflowExecutionRawHistoryV2Response {
    #[prost(message, repeated, tag = "2")]
    history_batches: Vec<DataBlob>,
    #[prost(message, optional, tag = "3")]
    version_history: Option<VersionHistory>,
}

#[derive(Clone, PartialEq, Message)]
struct AdminDeleteWorkflowExecutionRequest {
    #[prost(string, tag = "1")]
    namespace: String,
    #[prost(message, optional, tag = "2")]
    execution: Option<WorkflowExecution>,
}

#[derive(Clone, PartialEq, Message)]
struct SyncLocalExecutionRequest {
    #[prost(string, tag = "1")]
    namespace: String,
    #[prost(message, optional, tag = "2")]
    execution: Option<WorkflowExecution>,
    #[prost(int32, tag = "3")]
    protocol_version: i32,
    #[prost(string, tag = "4")]
    local_server_id: String,
    #[prost(string, tag = "5")]
    sync_id: String,
    #[prost(int64, tag = "6")]
    previous_event_id: i64,
    #[prost(int64, tag = "7")]
    previous_event_version: i64,
    #[prost(int64, tag = "8")]
    new_event_id: i64,
    #[prost(int64, tag = "9")]
    new_event_version: i64,
    #[prost(message, repeated, tag = "10")]
    history_batches: Vec<DataBlob>,
    #[prost(message, optional, tag = "11")]
    version_history: Option<VersionHistory>,
    #[prost(bool, tag = "12")]
    release: bool,
    #[prost(bytes = "vec", tag = "13")]
    ownership_token: Vec<u8>,
    #[prost(int64, tag = "14")]
    fencing_epoch: i64,
}

#[derive(Clone, PartialEq, Message)]
struct SyncLocalExecutionResponse {
    #[prost(string, tag = "1")]
    sync_id: String,
    #[prost(int64, tag = "2")]
    acknowledged_event_id: i64,
    #[prost(int64, tag = "3")]
    acknowledged_event_version: i64,
    #[prost(message, optional, tag = "4")]
    lease_expiration_time: Option<prost_types::Timestamp>,
}
