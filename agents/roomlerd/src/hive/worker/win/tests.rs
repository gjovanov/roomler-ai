// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use std::os::windows::io::IntoRawHandle;
use std::sync::Arc;

use bson::oid::ObjectId;
use tokio::net::windows::named_pipe::ClientOptions;
use windows_sys::Win32::System::JobObjects::IsProcessInJob;

use super::super::{
    GiveUp, Msg, Running, Spawn, Theirs, Timing, VERSION, Worker, recv, send, serve,
};
use super::*;

/// FR-90 P8-2b-2 — the guard arms `SEM_NOGPFAULTERRORBOX`, so a crash writes
/// no dump, event or upload of the worker's memory; and it keeps the other
/// error-mode bits its launcher may have set, rather than clearing them.
#[test]
fn no_crash_dump_arms_the_error_mode() {
    // SAFETY: GetErrorMode/SetErrorMode on this (the test) process.
    let before = unsafe { GetErrorMode() };
    // Clear the bit first, so this shows the GUARD sets it, not that the
    // launcher left it on: a Rust process on a dev box often already carries
    // it, while the daemon under the SCM does not. Keep the other bits, to
    // prove the guard OR's its bit in rather than overwriting.
    // SAFETY: as above.
    unsafe { SetErrorMode(before & !SEM_NOGPFAULTERRORBOX) };
    let cleared = unsafe { GetErrorMode() };
    assert_eq!(
        cleared & SEM_NOGPFAULTERRORBOX,
        0,
        "cleared before the guard runs: {cleared:#x}"
    );

    no_crash_dump().expect("the mode takes");

    let after = unsafe { GetErrorMode() };
    assert!(
        after & SEM_NOGPFAULTERRORBOX != 0,
        "the GP-fault report is suppressed: {after:#x}"
    );
    assert_eq!(
        after & cleared,
        cleared,
        "the bits it had stay, not cleared: cleared {cleared:#x}, after {after:#x}"
    );
    // Restore, so a later test's own crash still reports as its harness expects.
    // SAFETY: as above.
    unsafe { SetErrorMode(before) };
}

const WAIT: Duration = Duration::from_secs(10);

/// The worker's end of a channel, taken as a worker takes it.
fn client_of(pipe: OwnedHandle) -> NamedPipeClient {
    // SAFETY: the client end `channel` opened, overlapped, owned nowhere else.
    unsafe { NamedPipeClient::from_raw_handle(pipe.into_raw_handle()) }.unwrap()
}

/// A worker's pipe connects its two ends at once, and a frame crosses it
/// either way; it is one instance, so no other client can open it.
#[tokio::test]
async fn a_workers_pipe_has_its_two_ends_alone() {
    let name = format!(r"\\.\pipe\roomler-hive-worker-test-{}", ObjectId::new());
    let (server, pipe) = channel_named(&name).unwrap();
    let mut client = client_of(pipe);
    server.connect().await.expect("connected as it was made");
    let (mut server_rd, mut server_wr) = tokio::io::split(server);
    let hello = Msg::Hello {
        session: ObjectId::new().to_hex(),
        fence: 3,
    };
    send(&mut server_wr, &hello).await.unwrap();
    assert_eq!(recv(&mut client).await.unwrap(), Some(hello));
    send(&mut client, &Msg::Beat { n: 1 }).await.unwrap();
    assert_eq!(
        recv(&mut server_rd).await.unwrap(),
        Some(Msg::Beat { n: 1 })
    );

    let other = ClientOptions::new().open(&name);
    assert!(other.is_err(), "a second client of a worker's pipe");
}

/// Workers as tasks of this process, each serving its pipe as
/// `roomlerd hive-worker` does.
struct OnPipe;

struct Task(tokio::task::JoinHandle<()>);

#[async_trait::async_trait]
impl Running for Task {
    fn pid(&self) -> Option<u32> {
        None
    }

    async fn kill(&mut self) {
        self.0.abort();
    }
}

impl Spawn for OnPipe {
    fn spawn(&self, theirs: Theirs) -> std::io::Result<Box<dyn Running>> {
        let client = client_of(theirs.pipe);
        let task = tokio::spawn(async move {
            let _ = serve(client, None).await;
        });
        Ok(Box::new(Task(task)))
    }
}

/// The daemon keeps a worker up over its pipe: the handshake and the
/// heartbeat as on Unix, and `bye` at its stop.
#[tokio::test]
async fn the_daemon_keeps_a_worker_up_over_its_pipe() {
    let nobody: GiveUp = Arc::new(|_| {});
    let w = Worker::start(
        ObjectId::new(),
        1,
        Arc::new(OnPipe),
        Timing::quick(),
        nobody,
    );
    let deadline = std::time::Instant::now() + WAIT;
    let mut seen = w.seen();
    while !(seen.up && seen.starts == 1) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        seen = w.seen();
    }
    assert!(seen.up, "up over its pipe: {seen:?}");
    // A few heartbeats, answered: still the one start.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = w.seen();
    assert_eq!((seen.starts, seen.gave_up), (1, false), "{seen:?}");
    w.stop().await;
}

/// A worker starts into a job of its own: in it before it runs, the job
/// ending what is in it as its last handle closes, each process held to
/// `MAX_MEMORY`; and its end, the job ended.
#[tokio::test]
async fn a_worker_starts_in_a_job_of_its_own_its_memory_bounded() {
    let (_server, pipe) = channel().unwrap();
    let ping = Path::new(r"C:\Windows\System32\PING.EXE");
    let args = ["-n", "30", "127.0.0.1"].map(String::from);
    let mut worker = spawn(ping, None, &args, pipe).unwrap();
    let job = worker.job.as_ref().unwrap().as_raw_handle() as HANDLE;
    let mut inside = 0;
    // SAFETY: both handles are live; `inside` is a valid out-param.
    let ok = unsafe { IsProcessInJob(worker.process.as_raw_handle() as HANDLE, job, &mut inside) };
    assert!(ok != 0 && inside != 0, "in its job");
    // SAFETY: all-zero is a valid out-buffer for this class.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is exactly the size passed, and outlives the call.
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    assert!(ok != 0);
    let flags = info.BasicLimitInformation.LimitFlags;
    assert!(
        flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0,
        "the job ends what is in it"
    );
    assert!(
        flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY != 0 && info.ProcessMemoryLimit == MAX_MEMORY,
        "each process held to its bound: {}",
        info.ProcessMemoryLimit
    );
    let process = worker.process.as_raw_handle() as HANDLE;
    let started = std::time::Instant::now();
    worker.kill().await;
    // SAFETY: the process handle lives in `worker`.
    let ended = unsafe { WaitForSingleObject(process, 0) };
    assert_eq!(ended, WAIT_OBJECT_0, "it ended with its job");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "at once, long before its own end: {:?}",
        started.elapsed()
    );
}

/// The real `roomlerd hive-worker`, started as the daemon starts it: it
/// finds its pipe, answers `hello` with its own pid at this build's version,
/// each beat, and ends at `bye`. Needs the binary, which a lib test does not
/// build: `ROOMLERD_WORKER_EXE=… cargo test … -- --ignored`.
#[tokio::test]
#[ignore = "needs a built roomlerd.exe with hive, named in ROOMLERD_WORKER_EXE"]
async fn the_real_worker_answers_over_its_pipe() {
    let exe = std::env::var_os("ROOMLERD_WORKER_EXE").expect("ROOMLERD_WORKER_EXE");
    let (server, pipe) = channel().unwrap();
    let args = [super::super::WORKER_SUBCOMMAND.to_string()];
    let mut worker = spawn(Path::new(&exe), None, &args, pipe).unwrap();
    // Connected, the hello written and its answer read: any of them failing
    // is a worker that is not there, and lands on the one assertion below.
    let connected = server.connect().await;
    let (mut rd, mut wr) = tokio::io::split(server);
    let hello = Msg::Hello {
        session: ObjectId::new().to_hex(),
        fence: 7,
    };
    let ready = match connected {
        Ok(()) if send(&mut wr, &hello).await.is_ok() => tokio::time::timeout(WAIT, recv(&mut rd))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten(),
        _ => None,
    };
    assert_eq!(
        ready,
        Some(Msg::Ready {
            pid: worker.pid,
            version: VERSION.into()
        }),
        "its own pid, at this build's version"
    );
    for n in 1..=3 {
        send(&mut wr, &Msg::Beat { n }).await.unwrap();
        loop {
            match tokio::time::timeout(WAIT, recv(&mut rd))
                .await
                .unwrap()
                .unwrap()
            {
                Some(Msg::Log { .. }) => continue,
                other => {
                    assert_eq!(other, Some(Msg::Beat { n }), "beat {n} answered");
                    break;
                }
            }
        }
    }
    send(&mut wr, &Msg::Bye).await.unwrap();
    let process = worker.process.as_raw_handle() as usize;
    let waited = tokio::task::spawn_blocking(move || {
        // SAFETY: the process handle lives in `worker` across the wait.
        unsafe { WaitForSingleObject(process as HANDLE, 10_000) }
    })
    .await
    .unwrap();
    assert_eq!(waited, WAIT_OBJECT_0, "it ended at bye");
    worker.kill().await;
}
