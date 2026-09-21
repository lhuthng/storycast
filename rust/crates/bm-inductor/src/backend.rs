//! Local backend lifecycle: start/stop the inductor + worker from the TUI.
//!
//! The TUI is only a client of the inductor API — with nothing behind it, it
//! is a nicely rendered empty dashboard. These helpers let it bootstrap a
//! solo backend itself: an inductor and a worker as detached background
//! processes with logs under `.bm/`, so the whole pipeline runs from the one
//! terminal the TUI owns.
//!
//! PID files (`.bm/inductor.pid`, `.bm/agent.pid`) record what *this TUI*
//! started locally. `X` goes further: it stops every worker in the cluster —
//! the local ones by PID file plus a sweep for strays, the remote ones over
//! ssh — because a half-stopped cluster silently keeps rendering.
//!
//! Starting works degraded-first: the backend goes up immediately, then each
//! box provisions in the background and joins as it becomes ready. A failing
//! box lands in Error with its reason — it never vetoes the rest, because the
//! scheduler only offers tasks to beating workers anyway.

mod lifecycle;
mod net;
mod process;

pub(crate) use lifecycle::{
    inductor_up, lan_blackout, start_backend, start_workers, stop_everywhere, stop_inductor,
};
pub(crate) use net::{api_port, is_local_addr, public_bind};
pub(crate) use process::local_workers_alive;
