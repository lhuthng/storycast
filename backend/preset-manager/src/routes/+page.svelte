<script lang="ts">
	import { bytes, when } from '#lib/format';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	const packBytes = $derived(
		data.inv.packs.reduce((n, p) => n + (p.latest?.files ?? []).reduce((s, f) => s + f.bytes, 0), 0)
	);
	const copyBytes = $derived(data.copies.reduce((n, c) => n + c.bytes, 0));
</script>

<svelte:head><title>Preset manager</title></svelte:head>

<h1 class="adm-h1">Preset manager</h1>

<div class="adm-grid adm-grid--cards">
	<section class="card">
		<h2>Bucket</h2>
		<div class="wrap" style="margin-bottom:var(--space)">
			<span class="chip" class:chip--live={data.inv.error === null} class:chip--warn={data.inv.error !== null}>
				{data.inv.error === null ? 'reachable' : 'unavailable'}
			</span>
			<span class="chip">{data.inv.objects} objects</span>
		</div>
		{#if data.inv.error}
			<p class="muted" style="margin:0">{data.inv.error}</p>
		{:else}
			<p class="muted" style="margin:0">
				{data.inv.packs.length} packs, {bytes(packBytes)} published in total.
			</p>
			<a class="btn btn--ghost btn--sm" href="/packs" style="margin-top:var(--space)">Browse packs</a>
		{/if}
	</section>

	<section class="card">
		<h2>Working copies</h2>
		<p class="muted" style="margin:0 0 var(--space)">
			{data.copies.length} downloaded under <span class="adm-money">.bm/packs</span>,
			{bytes(copyBytes)} on disk.
		</p>
		{#if data.copies.length}
			<ul class="stack" style="list-style:none;margin:0;padding:0">
				{#each data.copies as c (c.rel)}
					<li>
						<a href="/packs/{c.kind}/{c.name}">{c.kind}/{c.name}</a>
						<span class="muted"> · {c.version} · {bytes(c.bytes)} · {c.files} files</span>
					</li>
				{/each}
			</ul>
		{:else}
			<p class="empty" style="margin:0">Nothing downloaded yet.</p>
		{/if}
	</section>

	<section class="card">
		<h2>Authored here</h2>
		{#if data.authors.length}
			<ul class="stack" style="list-style:none;margin:0;padding:0">
				{#each data.authors as a (a.name)}
					<li class="wrap">
						<span class="adm-money">{a.name}</span>
						<span class="muted">{a.files} files · {bytes(a.bytes)}</span>
						{#if a.bundled}
							<span class="chip chip--live">bundled</span>
						{:else}
							<span class="chip">no bundle</span>
						{/if}
					</li>
				{/each}
			</ul>
		{:else}
			<p class="empty" style="margin:0">No packs under assets/_extends/.</p>
		{/if}
	</section>
</div>

<section class="card" style="margin-top:var(--space)">
	<h2>Bundles ready to publish</h2>
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
									Publish
								</a>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{:else}
		<p class="empty" style="margin:0">
			None built yet. Build one from <a href="/release">Release</a>.
		</p>
	{/if}
</section>

<section class="card" style="margin-top:var(--space)">
	<h2>Local preset files</h2>
	<div class="wrap">
		{#each data.files as f (f.id)}
			<a class="chip" href="/files?id={f.id}">{f.label} · {bytes(f.bytes)}</a>
		{/each}
	</div>
</section>
