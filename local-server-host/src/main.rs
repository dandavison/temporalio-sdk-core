//! Drives the local-server wasm module (built from the wasmpoc/ tree of the temporal branch
//! dandavison/temporalio-temporal:local-workflow-progress) through one workflow (an activity and a timer) and reports
//! per-call latency and peak RSS.
use anyhow::{Result, bail};
use prost::Message;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use temporalio_protos::temporal::api::{
    command::v1::{
        Command, CompleteWorkflowExecutionCommandAttributes, ScheduleActivityTaskCommandAttributes,
        StartTimerCommandAttributes, command,
    },
    common::v1::{ActivityType, Payload, Payloads, WorkflowExecution, WorkflowType},
    enums::v1::{CommandType, EventType},
    taskqueue::v1::TaskQueue,
    workflowservice::v1::*,
};
use wasmtime::{Engine, Instance, Linker, Module, Store, TypedFunc};
use wasmtime_wasi::{WasiCtxBuilder, p1::WasiP1Ctx};

struct Guest {
    store: Store<WasiP1Ctx>,
    memory: wasmtime::Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<u32, ()>,
    call: TypedFunc<(u32, u32, u32, u32), u64>,
    advance_time: TypedFunc<i64, u64>,
    calls: Vec<(String, Duration)>,
}

impl Guest {
    fn new(path: &str, now_nanos: i64) -> Result<Self> {
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
            calls: vec![],
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<u32> {
        let ptr = self.alloc.call(&mut self.store, bytes.len() as u32)?;
        self.memory.write(&mut self.store, ptr as usize, bytes)?;
        Ok(ptr)
    }

    fn read_response(&mut self, packed: u64) -> Result<Vec<u8>> {
        let (ptr, len) = ((packed >> 32) as u32, (packed & 0xffff_ffff) as usize);
        let mut out = vec![0u8; len];
        self.memory.read(&self.store, ptr as usize, &mut out)?;
        self.free.call(&mut self.store, ptr)?;
        if out[0] != 0 {
            bail!("status {}: {}", out[0], String::from_utf8_lossy(&out[1..]));
        }
        Ok(out[1..].to_vec())
    }

    fn rpc<Req: Message, Resp: Message + Default>(
        &mut self,
        method: &str,
        request: &Req,
    ) -> Result<Resp> {
        let start = Instant::now();
        let method_ptr = self.write(method.as_bytes())?;
        let request_ptr = self.write(&request.encode_to_vec())?;
        let packed = self.call.call(
            &mut self.store,
            (
                method_ptr,
                method.len() as u32,
                request_ptr,
                request.encoded_len() as u32,
            ),
        )?;
        self.free.call(&mut self.store, method_ptr)?;
        self.free.call(&mut self.store, request_ptr)?;
        let response = Resp::decode(self.read_response(packed)?.as_slice())?;
        self.calls.push((method.to_string(), start.elapsed()));
        Ok(response)
    }

    fn advance_time(&mut self, now_nanos: i64) -> Result<()> {
        let packed = self.advance_time.call(&mut self.store, now_nanos)?;
        self.read_response(packed).map(|_| ())
    }
}

fn payloads(s: &str) -> Option<Payloads> {
    Some(Payloads {
        payloads: vec![Payload {
            metadata: [("encoding".to_string(), b"json/plain".to_vec())].into(),
            data: format!("{s:?}").into_bytes(),
            ..Default::default()
        }],
    })
}

fn task_queue() -> Option<TaskQueue> {
    Some(TaskQueue {
        name: "tq".into(),
        ..Default::default()
    })
}

fn poll_wft(g: &mut Guest) -> Result<PollWorkflowTaskQueueResponse> {
    g.rpc(
        "PollWorkflowTaskQueue",
        &PollWorkflowTaskQueueRequest {
            namespace: "default".into(),
            task_queue: task_queue(),
            identity: "host".into(),
            ..Default::default()
        },
    )
}

fn complete_wft(g: &mut Guest, token: Vec<u8>, commands: Vec<Command>) -> Result<()> {
    g.rpc::<_, RespondWorkflowTaskCompletedResponse>(
        "RespondWorkflowTaskCompleted",
        &RespondWorkflowTaskCompletedRequest {
            namespace: "default".into(),
            task_token: token,
            commands,
            identity: "host".into(),
            ..Default::default()
        },
    )?;
    Ok(())
}

fn rss_mb() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage.ru_maxrss as f64 / (1024.0 * 1024.0) // bytes on macOS
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: local-server-host <module.wasm|module.cwasm>");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as i64;
    let load = Instant::now();
    let mut g = Guest::new(&path, now)?;
    println!("load+init: {:?}", load.elapsed());

    let started: StartWorkflowExecutionResponse = g.rpc(
        "StartWorkflowExecution",
        &StartWorkflowExecutionRequest {
            namespace: "default".into(),
            workflow_id: "wf".into(),
            workflow_type: Some(WorkflowType {
                name: "Greet".into(),
            }),
            task_queue: task_queue(),
            input: payloads("world"),
            ..Default::default()
        },
    )?;

    let wft = poll_wft(&mut g)?;
    complete_wft(
        &mut g,
        wft.task_token,
        vec![
            Command {
                command_type: CommandType::ScheduleActivityTask as i32,
                attributes: Some(command::Attributes::ScheduleActivityTaskCommandAttributes(
                    ScheduleActivityTaskCommandAttributes {
                        activity_id: "1".into(),
                        activity_type: Some(ActivityType {
                            name: "Hello".into(),
                        }),
                        task_queue: task_queue(),
                        input: payloads("world"),
                        start_to_close_timeout: Some(prost_types::Duration {
                            seconds: 10,
                            nanos: 0,
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            Command {
                command_type: CommandType::StartTimer as i32,
                attributes: Some(command::Attributes::StartTimerCommandAttributes(
                    StartTimerCommandAttributes {
                        timer_id: "t1".into(),
                        start_to_fire_timeout: Some(prost_types::Duration {
                            seconds: 5,
                            nanos: 0,
                        }),
                    },
                )),
                ..Default::default()
            },
        ],
    )?;

    let act: PollActivityTaskQueueResponse = g.rpc(
        "PollActivityTaskQueue",
        &PollActivityTaskQueueRequest {
            namespace: "default".into(),
            task_queue: task_queue(),
            identity: "host".into(),
            ..Default::default()
        },
    )?;
    g.rpc::<_, RespondActivityTaskCompletedResponse>(
        "RespondActivityTaskCompleted",
        &RespondActivityTaskCompletedRequest {
            namespace: "default".into(),
            task_token: act.task_token,
            result: payloads("hello world"),
            ..Default::default()
        },
    )?;

    let wft = poll_wft(&mut g)?;
    g.advance_time(now + 5_000_000_000)?;
    complete_wft(&mut g, wft.task_token, vec![])?;

    let wft = poll_wft(&mut g)?;
    complete_wft(
        &mut g,
        wft.task_token,
        vec![Command {
            command_type: CommandType::CompleteWorkflowExecution as i32,
            attributes: Some(
                command::Attributes::CompleteWorkflowExecutionCommandAttributes(
                    CompleteWorkflowExecutionCommandAttributes {
                        result: payloads("done"),
                    },
                ),
            ),
            ..Default::default()
        }],
    )?;

    let history: GetWorkflowExecutionHistoryResponse = g.rpc(
        "GetWorkflowExecutionHistory",
        &GetWorkflowExecutionHistoryRequest {
            namespace: "default".into(),
            execution: Some(WorkflowExecution {
                workflow_id: "wf".into(),
                run_id: started.run_id,
            }),
            ..Default::default()
        },
    )?;
    for event in &history.history.unwrap().events {
        println!(
            "{:>3} {:?}",
            event.event_id,
            EventType::try_from(event.event_type)?
        );
    }
    for (method, elapsed) in &g.calls {
        println!("{elapsed:>12.3?} {method}");
    }
    println!("peak RSS: {:.1} MB", rss_mb());
    Ok(())
}
