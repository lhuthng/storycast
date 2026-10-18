<script lang="ts">
	import { enhance } from '$app/forms';
	import { bytes, when } from '#lib/format';
	import type { PageProps } from './$types';

	let { data, form }: PageProps = $props();
</script>

<svelte:head><title>Release</title></svelte:head>

<h1 class="adm-h1">Release</h1>

{#if form?.log}
	<div class="banner" class:banner--ok={form.ok} class:banner--err={!form.ok}>
		<strong>{form.what}: {form.ok ? 'done' : 'failed'}</strong>
		<pre class="log">{form.log}</pre>
	</div>
{/if}

<div class="adm-grid adm-grid--cards">
	<section class="card">
		<h2>Build the bundle</h2>
		<p class="adm-sub">
			<span class="adm-money">tools/profile.sh pack &lt;name&gt; --dep</span> packs
			<span class="adm-money">assets/_extends/&lt;name&gt;</span> into
			<span class="adm-money">profiles/pack/&lt;name&gt;.tar.zst</span>.
		</p>
		<form method="POST" action="?/build" use:enhance>
			<label class="adm-field">
				<span class="adm-label">Pack</span>
				<input
					class="adm-input adm-code"
					name="name"
					value={data.buildName}
					list="authors"
					placeholder="xianxia"
					required
				/>
			</label>
			<datalist id="authors">
				{#each data.authors as a (a.name)}
					<option value={a.name}>{a.deps.length ? `composed: ${a.deps.join(', ')}` : 'root pack'}</option>
				{/each}
			</datalist>
			<div class="wrap" style="margin-top:var(--space)">
				<button class="btn btn--primary" type="submit">Build</button>
				<label class="wrap muted" style="gap:var(--tight)">
					<input type="checkbox" name="dep" checked />
					release assets/_extends/&lt;name&gt; (--dep)
				</label>
				<label class="wrap muted" style="gap:var(--tight)">
					<input type="checkbox" name="force" />
					force, past a moved dependency
				</label>
			</div>
			<p class="muted" style="margin:var(--space) 0 0">
				--dep releases a pack of its own and is refused for a composed one. A dependency that has
				moved is refused unless forced.
			</p>
		</form>
	</section>

	<section class="card">
		<h2>Build the models bundle</h2>
		<p class="adm-sub">
			Gates the bake, then packs <span class="adm-money">engines/&lt;engine&gt;/models/manifest.json</span>
			and every weight it lists into
			<span class="adm-money">engines/&lt;engine&gt;/models/models.tar.zst</span>.
		</p>
		<form method="POST" action="?/buildModels" use:enhance>
			<div class="adm-fields">
				<label class="adm-field">
					<span class="adm-label">Engine</span>
					<select class="adm-select" name="engine">
						{#if !data.engines.some((e) => e.engine === data.engine)}
							<option value={data.engine}>{data.engine || 'select'}</option>
						{/if}
						{#each data.engines as e (e.engine)}
							<option value={e.engine} selected={data.engine === e.engine}>
								{e.engine} — {e.weights} weights
							</option>
						{/each}
					</select>
				</label>
				<label class="adm-field">
					<span class="adm-label">Level</span>
					<input class="adm-input adm-code" name="level" value="3" />
					<span class="muted">zstd level. 3 is the default; higher buys little on fp32 graphs.</span>
				</label>
			</div>
			<div class="wrap" style="margin-top:var(--space)">
				<button class="btn btn--primary" type="submit">Build models</button>
			</div>
			<p class="muted" style="margin:var(--space) 0 0">
				Refuses unless every file the manifest lists is on disk with the sha256 it records —
				a bundle cut over a drifted bake ships bytes its own manifest disagrees with.
			</p>
		</form>
		<form method="POST" action="?/verifyModels" use:enhance style="margin-top:var(--space)">
			<input type="hidden" name="engine" value={data.engine} />
			<button class="btn btn--ghost" type="submit">Verify the bundle</button>
			<span class="muted">Unpacks it and holds it to the manifest inside.</span>
		</form>
	</section>

	<section class="card">
		<h2>Publish it</h2>
		<p class="adm-sub">
			Runs <span class="adm-money">tools/preset-publish.sh</span>: uploads
			<span class="adm-money">preset/&lt;kind&gt;/&lt;name&gt;/&lt;version&gt;/</span> and repoints latest.json.
		</p>
		<form method="POST" action="?/publish" use:enhance>
			<div class="adm-fields">
				<label class="adm-field">
					<span class="adm-label">Kind</span>
					<select class="adm-select" name="kind">
						{#if !data.kinds.includes(data.kind)}<option value={data.kind}>{data.kind || 'select'}</option>{/if}
						{#each data.kinds as k (k)}<option value={k} selected={data.kind === k}>{k}</option>{/each}
					</select>
				</label>
				<label class="adm-field">
					<span class="adm-label">Name</span>
					<input class="adm-input adm-code" name="name" value={data.name} placeholder="xianxia" required />
				</label>
				<label class="adm-field">
					<span class="adm-label">Version</span>
					<input class="adm-input adm-code" name="version" value={data.version} />
					<span class="muted">Part of the key. `auto` stamps today.</span>
				</label>
				<label class="adm-field">
					<span class="adm-label">Archive</span>
					<input class="adm-input adm-code" name="bundle" value={data.bundle} list="bundles" required />
				</label>
			</div>
			<datalist id="bundles">
				{#each data.bundles as b (b.rel)}<option value={b.rel}></option>{/each}
			</datalist>
			<button class="btn btn--primary" type="submit" style="margin-top:var(--space)">Publish to R2</button>
		</form>
	</section>
</div>

<section class="card" style="margin-top:var(--space)">
	<h2>Built archives</h2>
	{#if data.bundles.length}
		<div class="adm-rows">
			<table class="adm-table">
				<thead>
					<tr><th>Archive</th><th class="adm-num">Size</th><th>Built</th><th class="adm-end"></th></tr>
				</thead>
				<tbody>
					{#each data.bundles as b (b.rel)}
						<tr>
							<td class="adm-money">{b.rel}</td>
							<td class="adm-money adm-num">{bytes(b.bytes)}</td>
							<td class="muted">{when(b.modified)}</td>
							<td class="adm-end">
								<a class="btn btn--ghost btn--sm" href="/release?bundle={encodeURIComponent(b.rel)}">
									Use
								</a>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{:else}
		<p class="empty">Nothing built yet.</p>
	{/if}
</section>

<section class="card" style="margin-top:var(--space)">
	<h2>Packs authored here</h2>
	{#if data.authors.length}
		<div class="adm-rows">
			<table class="adm-table">
				<thead>
					<tr>
						<th>Pack</th><th>Shape</th><th class="adm-num">Files</th><th class="adm-num">Size</th><th>Bundle</th>
					</tr>
				</thead>
				<tbody>
					{#each data.authors as a (a.name)}
						<tr>
							<td class="adm-money">{a.name}</td>
							<td>{a.deps.length ? `composed: ${a.deps.join(', ')}` : 'root pack'}</td>
							<td class="adm-money adm-num">{a.files}</td>
							<td class="adm-money adm-num">{bytes(a.bytes)}</td>
							<td>
								{#if a.bundled}<span class="chip chip--live">built</span>
								{:else}<span class="chip">none</span>{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{:else}
		<p class="empty">No packs under assets/_extends/.</p>
	{/if}
</section>
