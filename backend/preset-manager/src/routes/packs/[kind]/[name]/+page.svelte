<script lang="ts">
	import { enhance } from '$app/forms';
	import { today, when } from '#lib/format';
	import type { PageProps } from './$types';

	let { data, form }: PageProps = $props();

	// Only the version latest.json pins can be verified, so only it is offered a download.
	const latestVersion = $derived(data.pack?.latest?.version ?? null);
	// Handed over as plain data: `#lib/sounds` reads the disk, so a component
	// cannot import it without dragging node:fs into the browser build.
	const layers = $derived(data.layers);

	// Force is the destructive half: the fetch deletes the working copy before
	// unpacking, and that copy is where the sounds editor writes.
	function confirmFetch(e: MouseEvent, version: string, downloaded: boolean) {
		if (!downloaded) return;
		const dir = `.bm/packs/${data.kind}/${data.name}/${version}`;
		const msg = `Fetch ${version} again? ${dir} is deleted first, so any sound edit made in it is lost.`;
		if (!confirm(msg)) e.preventDefault();
	}
</script>

<svelte:head><title>{data.name}</title></svelte:head>

<a class="adm-back" href="/packs">← All packs</a>

<div class="wrap" style="margin-bottom:var(--tight)">
	<h1 class="adm-h1" style="margin:0">{data.name}</h1>
	<span class="chip">{data.kind}</span>
	{#if latestVersion}<span class="chip chip--live">latest {latestVersion}</span>{/if}
	{#each layers as p (p.id)}
		<a class="chip" href="/packs/{data.kind}/{data.name}/{p.id}">{p.label}</a>
	{/each}
</div>

{#if layers.length}
	<p class="muted" style="margin:0 0 var(--space)">
		Sounds edit <span class="adm-money">{data.tree?.label ?? 'this checkout'}</span>.
	</p>
{/if}

{#if form?.log}
	<div class="banner" class:banner--ok={form.ok} class:banner--err={!form.ok}>
		<strong>{form.what}: {form.ok ? 'done' : 'failed'}</strong>
		<pre class="log">{form.log}</pre>
	</div>
{/if}

{#if data.error}
	<div class="banner banner--err">{data.error}</div>
{:else}
	<div class="adm-grid">
		<section class="card">
			<div class="wrap" style="margin-bottom:var(--space)">
				<h2 style="margin:0">Versions</h2>
				<a
					class="btn btn--ghost btn--sm"
					href="/packs/{data.kind}/{data.name}?refresh=1"
					style="margin-left:auto">Refresh</a
				>
			</div>
			<div class="adm-rows">
				<table class="adm-table">
					<thead>
						<tr>
							<th>Version</th><th>Uploaded</th><th>State</th>
							<th class="adm-end"></th>
						</tr>
					</thead>
					<tbody>
						{#each data.pack?.versions ?? [] as v (v.version)}
							<tr>
								<td class="adm-money">{v.version}</td>
								<td class="muted">{when(v.modified)}</td>
								<td>
									{#if v.version === latestVersion}<span class="chip chip--live">latest</span>{/if}
									{#if v.downloaded}<span class="chip">here</span>{/if}
								</td>
								<td class="adm-end">
									{#if v.version === latestVersion}
										<form method="POST" action="?/download" use:enhance>
											<input type="hidden" name="kind" value={data.kind} />
											<input type="hidden" name="name" value={data.name} />
											{#if v.downloaded}<input type="hidden" name="force" value="on" />{/if}
											<button
												class="btn btn--ghost btn--sm"
												type="submit"
												onclick={(e) => confirmFetch(e, v.version, v.downloaded)}
											>
												{v.downloaded ? 'Fetch again' : 'Download'}
											</button>
										</form>
									{:else}
										<span class="muted">no pinned hash</span>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</section>

		<section class="card">
			<h2>Publish a new version</h2>
			<p class="adm-sub">Runs <span class="adm-money">tools/preset-publish.sh</span>.</p>

			{#if data.kind === 'sounds'}
				{#if data.authorDirExists && data.author?.deps.length}
					<!-- A dependency release is flat, one pack per directory, so `--dep`
					     refuses a composed tree. Packing it without the flag would name
					     the composition after this pack — the one outcome worse than a
					     refusal — so the button is not offered at all. -->
					<p class="banner" style="margin-bottom:var(--space)">
						<span class="adm-money">assets/_extends/{data.name}</span> is composed
						({data.author.deps.join(', ')}), and a dependency release is flat. Release the
						composition that names it instead, or name its roots in that composition's deps.
					</p>
				{:else}
					<form method="POST" action="?/build" use:enhance style="margin-bottom:var(--space)">
						<input type="hidden" name="name" value={data.name} />
						<p class="muted" style="margin:0 0 var(--tight)">
							{#if data.authorDirExists}
								<span class="adm-money">assets/_extends/{data.name}</span>, root pack
							{:else}
								<span class="adm-money">assets/</span>, the live composition of this checkout
							{/if}
						</p>
						<div class="wrap">
							<button class="btn btn--ghost btn--sm" type="submit">Rebuild the bundle</button>
							<label class="wrap muted" style="gap:var(--tight)">
								<input type="checkbox" name="dep" checked={data.authorDirExists} />
								--dep (root packs only)
							</label>
							<label class="wrap muted" style="gap:var(--tight)">
								<input type="checkbox" name="force" />
								force, past a moved dependency
							</label>
						</div>
					</form>
				{/if}
			{/if}

			<form method="POST" action="?/publish" use:enhance>
				<input type="hidden" name="kind" value={data.kind} />
				<input type="hidden" name="name" value={data.name} />
				<div class="adm-fields">
					<label class="adm-field">
						<span class="adm-label">Version</span>
						<input class="adm-input" name="version" value={today()} />
					</label>
					<label class="adm-field">
						<span class="adm-label">Archive to publish</span>
						<input class="adm-input adm-code" name="bundle" value={data.preferred} list="bundles" />
					</label>
				</div>
				<datalist id="bundles">
					{#each data.bundles as b (b)}<option value={b}></option>{/each}
				</datalist>
				<button class="btn btn--primary" type="submit" style="margin-top:var(--space)">Publish to R2</button>
				{#if data.bundles.length === 0}
					<p class="muted" style="margin:var(--space) 0 0">No archive built yet.</p>
				{/if}
			</form>
		</section>

		{#if data.kind === 'sounds'}
			<section class="card">
				<h2>Moods</h2>
				<p class="adm-sub">
					What a script names in <span class="adm-money">segments[].music</span>. A rename is
					recorded in <span class="adm-money">{data.historyFile}</span> and travels with the next
					bundle, so every workspace rewrites its own scripts on update.
				</p>

				{#if data.paletteError}
					<div class="banner banner--err">{data.paletteError}</div>
				{/if}

				<div class="adm-rows">
					<table class="adm-table">
						<thead>
							<tr>
								<th>Mood</th><th>Tags</th><th class="adm-end">Rename</th>
							</tr>
						</thead>
						<tbody>
							{#each data.moods as m (m.mood)}
								<tr>
									<td>
										<span class="adm-money">{m.mood}</span>
										{#if m.id}<span class="chip">was {m.id}</span>{/if}
										{#if m.note}<div class="muted">{m.note}</div>{/if}
									</td>
									<td class="muted">{m.tags.join(', ') || '—'}</td>
									<td class="adm-end">
										{#if m.mood === 'none'}
											<span class="muted">silence</span>
										{:else}
											<form method="POST" action="?/renameMood" use:enhance class="wrap">
												<input type="hidden" name="mood" value={m.mood} />
												<input
													class="adm-input adm-code"
													name="rename:{m.mood}"
													placeholder="new name"
												/>
												<button class="btn btn--ghost btn--sm" type="submit">Rename</button>
											</form>
										{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>

				{#if data.renames.length}
					<p class="adm-sub" style="margin-top:var(--space)">Declared so far</p>
					<ul class="muted" style="margin:0;padding-left:1.2em">
						{#each data.renames as r (r)}<li class="adm-code">{r}</li>{/each}
					</ul>
				{/if}
			</section>
		{/if}
	</div>
{/if}
