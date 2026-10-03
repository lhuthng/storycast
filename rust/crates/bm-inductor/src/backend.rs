//! Local backend lifecycle: start/stop the inductor + worker from the TUI.

mod lifecycle;
mod net;
mod process;

pub(crate) use lifecycle::{
    inductor_up, lan_blackout, start_backend, start_workers, stop_everywhere, stop_inductor,
};
pub(crate) use net::{api_port, is_local_addr, public_bind};
pub(crate) use process::local_workers_alive;
