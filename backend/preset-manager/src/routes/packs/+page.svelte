<script lang="ts">
	import { enhance } from '$app/forms';
	import { bytes } from '#lib/format';
	import type { PageProps } from './$types';

	let { data, form }: PageProps = $props();

	const size = (files: { bytes: number }[]) => files.reduce((n, f) => n + f.bytes, 0);
</script>

<svelte:head><title>Packs</title></svelte:head>

<h1 class="adm-h1">Packs</h1>

{#if form?.log}
	<div class="banner" class:banner--ok={form.ok} class:banner--err={!form.ok}>
		<strong>{form.pack}</strong>
		<pre class="log">{form.log}</pre>
	</div>
{/if}

{#if data.error}
	<div class="banner banner--err">
		The bucket could not be listed: {data.error}
		<p class="muted" style="margin:var(--tight) 0 0">
			Fill <span class="adm-money">R2_ACCOUNT_ID</span>,
			<span class="adm-money">R2_ACCESS_KEY_ID</span> and
			<span class="adm-money">R2_SECRET_ACCESS_KEY</span> in the checkout's .env, or export them where
			the dev server runs. Listing needs the AWS CLI; a download does not.
		</p>
	</div>
{:else}
	<form method="GET" class="wrap" style="margin-bottom:var(--space)">
		<label class="adm-field" style="max-width:16rem">
			<span class="adm-label">Kind</span>
			<select class="adm-select" name="kind">
				<option value="">All kinds</option>
				{#each data.kinds as k (k)}<option value={k} selected={data.kind === k}>{k}</option>{/each}
			</select>
		</label>
		<button class="btn btn--primary" type="submit" style="align-self:end">Filter</button>
		<span class="muted" style="align-self:end">{data.packs.length} packs</span>
		<a
			class="btn btn--ghost btn--sm"
			href={data.kind ? `/packs?refresh=1&kind=${data.kind}` : '/packs?refresh=1'}
			style="align-self:end">Refresh</a
		>
	</form>

	{#if data.packs.length === 0}
		<p class="empty">Nothing published under this filter.</p>
	{:else}
		<div class="adm-cards">
			{#each data.packs as p (p.kind + '/' + p.name)}
				<div class="adm-row">
					<div class="wrap" style="justify-content:space-between">
						<a href="/packs/{p.kind}/{p.name}" style="font-weight:600">{p.name}</a>
						<span class="chip">{p.kind}</span>
					</div>
					<p class="muted" style="margin:var(--tight) 0 0">
						{p.latest?.version ?? 'none'} · {p.latest ? bytes(size(p.latest.files)) : 'none'}{#if p.versions.some(
							(v) => v.downloaded
						)}
							· downloaded here{/if}
					</p>
					<form method="POST" action="?/download" use:enhance style="margin-top:var(--space)">
						<input type="hidden" name="kind" value={p.kind} />
						<input type="hidden" name="name" value={p.name} />
						<button class="btn btn--ghost btn--sm" type="submit">Download latest</button>
					</form>
				</div>
			{/each}
		</div>

		<div class="adm-rows adm-wide">
			<table class="adm-table">
				<thead>
					<tr>
						<th>Pack</th>
						<th>Kind</th>
						<th>Latest</th>
						<th class="adm-num">Files</th>
						<th class="adm-num">Size</th>
						<th>Here</th>
						<th class="adm-end"></th>
					</tr>
				</thead>
				<tbody>
					{#each data.packs as p (p.kind + '/' + p.name)}
						<tr>
							<td><a href="/packs/{p.kind}/{p.name}">{p.name}</a></td>
							<td><span class="chip">{p.kind}</span></td>
							<td class="adm-money">
								{p.latest?.version ?? 'none'}
								{#if p.latestError}<span class="muted" title={p.latestError}> (latest.json unreadable)</span>{/if}
							</td>
							<td class="adm-money adm-num">{p.latest?.files.length ?? 0}</td>
							<td class="adm-money adm-num">{p.latest ? bytes(size(p.latest.files)) : 'none'}</td>
							<td>
								{#each p.versions as v (v.version)}
									{#if v.downloaded}<span class="chip chip--live">{v.version}</span>{/if}
								{/each}
							</td>
							<td class="adm-end">
								<form method="POST" action="?/download" use:enhance>
									<input type="hidden" name="kind" value={p.kind} />
									<input type="hidden" name="name" value={p.name} />
									<button class="btn btn--ghost btn--sm" type="submit">Download latest</button>
								</form>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
{/if}
