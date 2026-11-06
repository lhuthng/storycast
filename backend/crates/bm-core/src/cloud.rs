//! What a cloud must answer, in terms no provider owns: a launch plan, the
//! boxes it produced, and the reason a box was not created.
pub mod instabox;
mod launch;
mod provider;
mod view;

pub use launch::{
    too_small, Cloud, CloudBox, Launch, Plan, Reason, Shortfall, Smoke, MIN_BOX_MIB,
    SIDECAR_RESIDENT_MIB,
};
pub use provider::Provider;
pub use view::{fate, Fate, Liveness, ProviderView, BEAT_WINDOW_SECS};

/// `--ttl 7h`, `90m`, `0` (no deadline) → minutes. A bare number is hours.
pub fn parse_ttl_minutes(raw: &str) -> Option<u32> {
    let s = raw.trim().to_ascii_lowercase();
    let (number, unit) = if let Some(n) = s.strip_suffix('h') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1)
    } else {
        (s.as_str(), 60)
    };
    let n: u32 = number.trim().parse().ok()?;
    n.checked_mul(unit)
}

/// A deadline in minutes written the way an operator reads it.
pub fn ttl_label(minutes: Option<u32>) -> String {
    match minutes {
        None => "default".into(),
        Some(0) => "no deadline".into(),
        Some(m) if m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{m}m"),
    }
}

#[cfg(test)]
mod tests;
