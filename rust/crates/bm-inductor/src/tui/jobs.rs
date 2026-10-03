//! Background work: one `Job` at a time, off the drawing loop.
use crate::tui::{
    app::App,
    input::{op_key, urlencode},
    style::{Level, LogLine},
};
use aws::job_aws_discover;
use aws::job_aws_down;
use aws::job_aws_login;
pub(crate) use aws::job_aws_pool;
use aws::job_aws_up;
use aws::job_relink_machine;
use backend::job_drop_machine;
use backend::job_save_task_policy;
use backend::job_set_accepting;
use backend::job_set_tts_threads;
use backend::job_stop_backend;
#[allow(unused_imports)]
pub(crate) use backend::{job_start_backend, split_catchup};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bm_proto::{Machine, MachineState, Op, OpRequest, Roster};
use data::job_llm_models;
use data::job_load_lines;
use data::job_load_sounds;
use data::job_op;
use data::job_preview_local;
pub(crate) use data::{job_load_roster, job_segment};
use digest::job_digest_policy;
use digest::job_manual_digest;
use provision::job_add_machine;
use provision::job_add_sample;
use provision::job_provision;
#[allow(unused_imports)]
pub(crate) use reqs::{
    fetch_state, op_job, send, set_machine_state, unreachable_verdict,
    verdict_after_failed_provision, DoneKind, Ev, Job, ProfileReq, WorkspaceReq,
};
use std::collections::{BTreeSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

pub(crate) struct BackgroundJob {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) queued: Instant,
    pub(crate) started: Option<Instant>,
    pub(crate) activity: String,
}

/// Something only one job at a time may hold.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Res {
    /// The default lane: every job with no real conflict with anything.
    Command,
    /// The backend and the worker fleet as a whole.
    Cluster,
    /// One machine's ssh/rsync channel.
    Box(String),
    /// The AWS account, and the `.bm/aws/` document it is written into.
    Aws,
}

impl Res {
    /// How the jobs screen names this resource. Short, it sits on one row.
    pub(crate) fn label(&self) -> String {
        match self {
            Res::Command => "the command lane".into(),
            Res::Cluster => "the cluster".into(),
            Res::Box(addr) => format!("box {addr}"),
            Res::Aws => "the aws account".into(),
        }
    }
}

pub(crate) async fn run_jobs(
    job_rx: tokio::sync::mpsc::UnboundedReceiver<Job>,
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
) {
    run_jobs_with(job_rx, tx, run_job).await;
}

pub(crate) async fn run_jobs_with<F, Fut>(
    mut job_rx: tokio::sync::mpsc::UnboundedReceiver<Job>,
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    runner: F,
) where
    F: Fn(Job, tokio::sync::mpsc::UnboundedSender<Ev>) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    // A job is queued only behind another job that holds something it needs.
    let mut pending: VecDeque<(Job, Vec<Res>)> = VecDeque::new();
    let mut busy: BTreeSet<Res> = BTreeSet::new();
    // Held by the scheduler as well, so a `recv` here never returns `None`
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<Res>>();
    let mut accepting = true;

    loop {
        // Start everything that can start, in arrival order. Re-scan from the
        let mut i = 0;
        while i < pending.len() {
            if pending[i].1.iter().any(|r| busy.contains(r)) {
                i += 1;
                continue;
            }
            let (job, res) = pending.remove(i).expect("index checked above");
            busy.extend(res.iter().cloned());
            let run = runner.clone();
            let tx = tx.clone();
            let done = done_tx.clone();
            tokio::spawn(async move {
                run_one(job, tx, run).await;
                let _ = done.send(res);
            });
        }
        if !accepting && pending.is_empty() && busy.is_empty() {
            return;
        }
        if accepting {
            tokio::select! {
                job = job_rx.recv() => match job {
                    Some(job) => {
                        let res = job.resources();
                        pending.push_back((job, res));
                    }
                    None => accepting = false,
                },
                Some(res) = done_rx.recv() => release(&mut busy, &res),
            }
        } else {
            match done_rx.recv().await {
                Some(res) => release(&mut busy, &res),
                None => return,
            }
        }
    }
}

fn release(busy: &mut BTreeSet<Res>, res: &[Res]) {
    for r in res {
        busy.remove(r);
    }
}

/// Run one job to completion on its own task: announce it, forward its events
async fn run_one<F, Fut>(job: Job, tx: tokio::sync::mpsc::UnboundedSender<Ev>, runner: F)
where
    F: Fn(Job, tokio::sync::mpsc::UnboundedSender<Ev>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let id = match &job {
        Job::Tracked { id, .. } => Some(*id),
        _ => None,
    };
    let fallback = job.fallback_done();
    if let Some(id) = id {
        let _ = tx.send(Ev::JobStarted(id));
    }
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut task = tokio::spawn(async move { runner(job.into_bare(), job_tx).await });
    let mut done = false;
    let mut forward = |ev: Ev| {
        if matches!(ev, Ev::Done(_)) {
            if done {
                return;
            }
            done = true;
        }
        if let (Some(id), Ev::Log(line)) = (id, &ev) {
            let _ = tx.send(Ev::JobProgress {
                id,
                text: line.text.clone(),
            });
        }
        let _ = tx.send(ev);
    };
    let result = loop {
        tokio::select! {
            result = &mut task => break result,
            Some(ev) = job_rx.recv() => forward(ev),
        }
    };
    job_rx.close();
    while let Some(ev) = job_rx.recv().await {
        forward(ev);
    }
    if result.is_err() {
        send(
            &tx,
            Level::Error,
            "background job crashed — retry the operation".into(),
        );
    }
    if !done {
        let _ = tx.send(Ev::Done(fallback));
    }
    if let Some(id) = id {
        let _ = tx.send(Ev::JobFinished(id));
    }
}

/// Refuse to move what the cluster is reading, or `None` when it is quiet.
async fn cluster_busy(api: &str) -> Option<String> {
    if crate::backend::inductor_up(api).await {
        return Some(
            "inductor is answering — :X first (it owns the ledger this would move)".into(),
        );
    }
    if crate::backend::local_workers_alive() {
        return Some("local workers still running — :X first, then try again".into());
    }
    None
}

/// The operator's advertised address for this cluster, if they set one.
fn advertised_host(layout: &bm_core::Layout) -> Option<String> {
    bm_core::config::Settings::load(&layout.settings())
        .advertised_host()
        .map(str::to_string)
}

pub(crate) async fn run_job(job: Job, tx: tokio::sync::mpsc::UnboundedSender<Ev>) {
    match job.into_bare() {
        Job::Tracked { .. } => unreachable!(),
        Job::Provision {
            layout,
            api,
            machine,
            force,
            settings_key,
            cancel,
        } => job_provision(tx, layout, api, machine, force, settings_key, cancel).await,
        Job::AddMachine { api, http, m } => job_add_machine(tx, api, http, m).await,
        Job::AwsPool { root, api, http } => job_aws_pool(tx, root, api, http).await,
        Job::AwsLogin { root, csv } => job_aws_login(tx, root, csv).await,
        Job::AwsDiscover { root, args } => job_aws_discover(tx, root, args).await,
        Job::AwsUp {
            root,
            api,
            http,
            count,
        } => job_aws_up(tx, root, api, http, count).await,
        Job::AwsDown { root, ids } => job_aws_down(tx, root, ids).await,
        Job::AddSample {
            layout,
            path,
            name,
            tags,
        } => job_add_sample(tx, layout, path, name, tags).await,
        Job::StartBackend {
            layout,
            api,
            api_up,
            start,
            count,
            enqueue,
            machines,
            cancel,
            settings_key,
        } => {
            job_start_backend(
                tx,
                layout,
                api,
                api_up,
                start,
                count,
                enqueue,
                machines,
                cancel,
                settings_key,
            )
            .await
        }
        Job::StopBackend {
            layout,
            machines,
            api,
            settings_key,
        } => job_stop_backend(tx, layout, machines, api, settings_key).await,
        Job::DropMachine { api, http, addr } => job_drop_machine(tx, api, http, addr).await,
        Job::SaveTaskPolicy {
            api,
            http,
            addr,
            task_policy,
        } => job_save_task_policy(tx, api, http, addr, task_policy).await,
        Job::SetAccepting {
            api,
            http,
            addr,
            accepting_work,
        } => job_set_accepting(tx, api, http, addr, accepting_work).await,
        Job::SetTtsThreads {
            api,
            http,
            addr,
            threads,
        } => job_set_tts_threads(tx, api, http, addr, threads).await,
        Job::ManualDigest {
            api,
            http,
            chapter,
            script,
            delta,
        } => job_manual_digest(tx, api, http, chapter, script, delta).await,
        Job::DigestPolicy {
            api,
            http,
            layout,
            machines,
            restore,
        } => job_digest_policy(tx, api, http, layout, machines, restore).await,
        Job::RelinkMachine {
            layout,
            api,
            http,
            machine,
        } => job_relink_machine(tx, layout, api, http, machine).await,
        Job::Op {
            api,
            http,
            req,
            layout,
        } => job_op(tx, api, http, req, layout).await,
        Job::LoadRoster { api, http, layout } => job_load_roster(tx, api, http, layout).await,
        Job::LoadLines { layout } => job_load_lines(tx, layout).await,
        Job::LoadSounds { layout } => job_load_sounds(tx, layout).await,
        Job::Segment {
            layout,
            character,
            voice,
            text,
        } => job_segment(tx, layout, character, voice, text).await,
        Job::PreviewLocal {
            layout,
            voice,
            text,
        } => job_preview_local(tx, layout, voice, text).await,
        Job::Workspace { layout, api, req } => job_workspace(tx, layout, api, req).await,
        Job::Profile { layout, api, req } => job_profile(tx, layout, api, req).await,
        Job::LlmModels {
            provider,
            kind,
            base_url,
            key,
        } => job_llm_models(tx, provider, kind, base_url, key).await,
    }
}

#[allow(unused_imports)]
pub(crate) use digest::digest_restore;
#[allow(unused_imports)]
pub(crate) use workspace::{job_profile, job_workspace};
mod aws;
mod backend;
mod data;
mod digest;
mod provision;
mod reqs;
mod workspace;
