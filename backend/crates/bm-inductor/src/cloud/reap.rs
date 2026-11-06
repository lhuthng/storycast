//! The reaper's cloud half: what each provider says about the boxes it
//! launched, the verdict built from that, and the deletes still owed to it.

use crate::api::Shared;
use crate::state::DeleteIntent;
use bm_core::cloud::{fate, Cloud, Fate, Liveness, Provider, ProviderView};
use bm_proto::now_secs;
use std::collections::BTreeMap;
use std::path::Path;

/// One provider's answer, with the boxes it was asked about.
struct Polled {
    name: String,
    rows: Vec<Row>,
    views: Result<Vec<ProviderView>, String>,
}

/// One box the registry says a provider owns.
struct Row {
    addr: String,
    provider_id: String,
    expires_at: Option<u64>,
    /// The newest beat any worker on this address filed, if any.
    last_beat: Option<u64>,
}

/// Poll every provider that owns a box, then settle one owed delete.
///
/// Nothing here kills a box on a failed poll: the only evidence that ends a box
/// is the provider's own word, our deadline, or its absence from an answer that
/// did arrive. Lines are for the operator.
pub(crate) async fn sweep(state: Shared, root: std::path::PathBuf) -> Vec<String> {
    let now = now_secs();
    let (groups, owed) = snapshot(&state).await;
    if groups.is_empty() && owed.is_none() {
        return Vec::new();
    }
    // Every provider call shells out, so the whole pass runs off the runtime.
    let (polls, settled) = tokio::task::spawn_blocking(move || {
        let polls: Vec<Polled> = groups
            .into_iter()
            .map(|(name, rows)| {
                let polled = poll(&name, &rows, &root);
                Polled {
                    name,
                    rows,
                    views: polled,
                }
            })
            .collect();
        let settled = owed.map(|i| {
            let outcome = delete(&i.provider, &i.provider_id, &root);
            (i.provider_id, outcome)
        });
        (polls, settled)
    })
    .await
    .unwrap_or_default();

    let mut out = Vec::new();
    let mut inner = state.lock().await;
    for Polled { name, rows, views } in polls {
        let views = match views {
            Ok(views) => views,
            Err(e) => {
                // A poll that failed is not evidence of anything.
                out.push(format!(
                    "{name}: could not poll ({e}) — nothing killed on a failed poll"
                ));
                continue;
            }
        };
        let by_id: BTreeMap<&str, &ProviderView> =
            views.iter().map(|v| (v.provider_id.as_str(), v)).collect();
        for row in &rows {
            // Absent from an answer that arrived: the provider no longer has it.
            let absent = ProviderView::new(
                row.provider_id.clone(),
                Liveness::Gone,
                "not in the provider's answer",
            );
            let view = by_id
                .get(row.provider_id.as_str())
                .copied()
                .unwrap_or(&absent);
            if let Fate::Dead { why } = fate(view, row.last_beat, row.expires_at, now) {
                if let Some(line) = inner.reclaim(&row.addr, &why) {
                    out.push(line);
                }
            }
        }
    }
    if let Some((provider_id, outcome)) = settled {
        if let Some(line) = inner.settle_intent(&provider_id, outcome) {
            out.push(line);
        }
    }
    out
}

/// The boxes each provider owns, and the oldest delete still owed.
async fn snapshot(state: &Shared) -> (Vec<(String, Vec<Row>)>, Option<DeleteIntent>) {
    let inner = state.lock().await;
    let mut groups: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for m in inner.machines.values() {
        let (Some(provider), Some(provider_id)) = (m.provider.clone(), m.provider_id.clone())
        else {
            continue;
        };
        let last_beat = inner
            .workers
            .iter()
            .filter(|(_, a)| a.as_str() == m.addr)
            .filter_map(|(w, _)| inner.beats.get(w).map(|b| b.ts))
            .max();
        groups.entry(provider).or_default().push(Row {
            addr: m.addr.clone(),
            provider_id,
            expires_at: m.expires_at,
            last_beat,
        });
    }
    (groups.into_iter().collect(), inner.next_intent())
}

fn poll(name: &str, rows: &[Row], root: &Path) -> Result<Vec<ProviderView>, String> {
    let provider = cloud_for(name, root)?;
    let ids: Vec<String> = rows.iter().map(|r| r.provider_id.clone()).collect();
    provider.list(&ids).map_err(|e| e.line())
}

fn delete(name: &str, id: &str, root: &Path) -> Result<(), String> {
    let provider = cloud_for(name, root)?;
    match provider.down(&[id.to_string()]) {
        Ok(_) => Ok(()),
        // The provider not knowing it is the outcome we asked for.
        Err(e) if e.code == "InvalidInstanceID.NotFound" => Ok(()),
        Err(e) => Err(e.line()),
    }
}

fn cloud_for(name: &str, root: &Path) -> Result<Box<dyn Cloud>, String> {
    let provider =
        Provider::parse(name).ok_or_else(|| format!("no provider is named `{name}` anymore"))?;
    super::for_provider(Some(provider), root, None).map_err(|e| format!("{e:#}"))
}
