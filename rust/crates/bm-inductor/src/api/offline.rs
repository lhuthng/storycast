use super::*;

/// Swap with no scheduler: the same `op_swap_voice` against a throwaway
/// Inner, which persists cast + ledger itself. Two locks before touching
/// anything: the inductor API must be down (its scheduler owns these files
/// while it answers), and no local worker may be alive (a mid-render worker
/// keeps rendering the old cast). Remote strays are the operator's
/// responsibility, the supported flow is X (which sweeps them), then swap.
pub(crate) async fn offline_swap(
    api: &str,
    layout: &bm_core::Layout,
    character: &str,
    voice: &str,
) -> Result<String, String> {
    if super::super::backend::inductor_up(api).await {
        return Err(
            "inductor is back — swap normally (this path is for inductor-down only)".into(),
        );
    }
    if super::super::backend::local_workers_alive() {
        return Err("local workers still running — X first, then swap".into());
    }
    offline_swap_apply(layout, character, voice)
}

/// The file mutation itself, minus the guards: throwaway Inner over disk
/// files, same `op_swap_voice` the live path runs (which persists cast +
/// ledger itself). Split out so tests can run it without a scheduler, a
/// network, or a worker-shaped hole in the room.
pub(crate) fn offline_swap_apply(
    layout: &bm_core::Layout,
    character: &str,
    voice: &str,
) -> Result<String, String> {
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    // The startup pass the live inductor runs before any op: an invalidation
    // is diffed against the recorded plan, so without one the swap could only
    // re-speak whole chapters. Built *before* the mutation, so it records the
    // inputs as they are now and the diff afterwards names what moved.
    inner.adopt_render_plans();
    inner
        .op_swap_voice(character, voice)
        .map(|m| format!("{m} [offline — inductor was down]"))
        .map_err(|e| e.to_string())
}

/// Remix with no scheduler: the same `op_remix` against a throwaway Inner,
/// which persists settings + ledger itself. Same guards as the swap path
/// the inductor API must be down and no local worker alive.
pub(crate) async fn offline_remix(
    api: &str,
    layout: &bm_core::Layout,
    speed: Option<f64>,
    effect_volume: Option<f64>,
    music_volume: Option<f64>,
    inject_volume: Option<f64>,
) -> Result<String, String> {
    if super::super::backend::inductor_up(api).await {
        return Err(
            "inductor is back — remix normally (this path is for inductor-down only)".into(),
        );
    }
    if super::super::backend::local_workers_alive() {
        return Err("local workers still running — X first, then remix".into());
    }
    offline_remix_apply(layout, speed, effect_volume, music_volume, inject_volume)
}

pub(crate) fn offline_remix_apply(
    layout: &bm_core::Layout,
    speed: Option<f64>,
    effect_volume: Option<f64>,
    music_volume: Option<f64>,
    inject_volume: Option<f64>,
) -> Result<String, String> {
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    inner
        .op_remix(speed, effect_volume, music_volume, inject_volume)
        .map(|m| format!("{m} [offline — inductor was down]"))
        .map_err(|e| e.to_string())
}

/// A sound-design write with no scheduler to notice it.
///
/// `:sound` writes the registries itself, so the write succeeds whether or not
/// the inductor is up, but the invalidation is the *scheduler's* work, and
/// without this path an edit made while the inductor was down would go
/// unnoticed until the next boot. That was survivable while adoption at boot
/// was the only mechanism; it is not survivable now that a boot can adopt an
/// unstamped merge, so the same op runs against a throwaway `Inner` here.
///
/// No `local_workers_alive` guard, unlike the swap and remix paths: those two
/// delete a voice's cached segments, which a running worker can be mid-write
/// on. This deletes published mp3s and requeues, the ordinary queue traffic.
pub(crate) async fn offline_sound_changed(
    api: &str,
    layout: &bm_core::Layout,
) -> Result<String, String> {
    if super::super::backend::inductor_up(api).await {
        return Err(
            "inductor is back — sound changes go through it (this path is for inductor-down only)"
                .into(),
        );
    }
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    Ok(format!(
        "{} [offline — inductor was down]",
        inner.op_sound_changed()
    ))
}
