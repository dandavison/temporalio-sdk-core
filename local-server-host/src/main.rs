//! Drives the local-server wasm module through one workflow (an activity and a timer), playing the
//! worker with hand-built requests, and reports per-call latency and peak RSS.
use anyhow::Result;
use local_server_host::Guest;
use prost::Message;
use std::{
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
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

struct Host {
    guest: Guest,
    calls: Vec<(String, Duration)>,
}

impl Host {
    fn rpc<Req: Message, Resp: Message + Default>(
        &mut self,
        method: &str,
        request: &Req,
    ) -> Result<Resp> {
        let start = Instant::now();
        let response = self.guest.call(method, &request.encode_to_vec())??;
        self.calls.push((method.to_string(), start.elapsed()));
        Ok(Resp::decode(response.as_slice())?)
    }

    fn advance_time(&mut self, now_nanos: i64) -> Result<()> {
        self.guest.advance_time(now_nanos).map(|_| ())
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

fn poll_wft(g: &mut Host) -> Result<PollWorkflowTaskQueueResponse> {
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

fn complete_wft(g: &mut Host, token: Vec<u8>, commands: Vec<Command>) -> Result<()> {
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
    let mut g = Host {
        guest: Guest::new(Path::new(&path), now)?,
        calls: vec![],
    };
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
