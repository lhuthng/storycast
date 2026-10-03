use super::*;

/// Swap with no scheduler: the same `op_swap_voice` against a throwaway
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
pub(crate) fn offline_swap_apply(
    layout: &bm_core::Layout,
    character: &str,
    voice: &str,
) -> Result<String, String> {
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    // The startup pass the live inductor runs before any op: an invalidation
    inner.adopt_render_plans();
    inner
        .op_swap_voice(character, voice)
        .map(|m| format!("{m} [offline — inductor was down]"))
        .map_err(|e| e.to_string())
}

/// Remix with no scheduler: the same `op_remix` against a throwaway Inner,
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
