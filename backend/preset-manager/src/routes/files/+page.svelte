<script lang="ts">
	import { enhance } from '$app/forms';
	import { bytes, when } from '#lib/format';
	import type { PageProps } from './$types';

	let { data, form }: PageProps = $props();

	let jsonError = $state<string | null>(null);
	const isPresets = $derived(data.file.id === 'presets');
	const keys = $derived((data.presets?.rows ?? []).map((r) => r.key).join('\n'));

	/** Save is only offered once the text is JSON a parser agrees with. */
	function check(value: string) {
		if (isPresets) return;
		try {
			JSON.parse(value);
			jsonError = null;
		} catch (e) {
			jsonError = (e as Error).message;
		}
	}
</script>

<svelte:head><title>{data.file.label}</title></svelte:head>

<h1 class="adm-h1">Local files</h1>
<p class="adm-sub">Read and written in place.</p>

{#if form?.msg}
	<div class="banner" class:banner--ok={form.ok} class:banner--err={!form.ok}>{form.msg}</div>
{/if}

<div class="adm-grid adm-grid--side">
	<nav class="card" aria-label="Files">
		<h2>Files</h2>
		<ul class="stack" style="list-style:none;margin:0;padding:0">
			{#each data.files as f (f.id)}
				<li>
					<a
						class="adm-row"
						href="/files?id={f.id}"
						aria-current={f.id === data.file.id ? 'page' : undefined}
						style="display:block;text-decoration:none;color:inherit;{f.id === data.file.id
							? 'border-color:var(--color-primary)'
							: ''}"
					>
						<strong>{f.label}</strong>
						<span class="muted" style="display:block">
							{#if f.exists}{bytes(f.bytes)} · {when(f.mtime)}{:else}not on disk{/if}
						</span>
					</a>
				</li>
			{/each}
		</ul>
	</nav>

	<section class="card">
		<h2>{data.file.label}</h2>
		<p class="adm-sub">
			{data.file.what}
			<span class="adm-money" style="display:block">{data.file.rel}</span>
		</p>

		{#if data.readError}
			<div class="banner banner--err">{data.readError}</div>
		{:else if isPresets && data.presets}
			<form method="POST" action="?/savePresets" use:enhance>
				<input type="hidden" name="keys" value={keys} />

				{#if data.presets.hasNote}
					<label class="adm-field" style="margin-bottom:var(--space)">
						<span class="adm-label">Note</span>
						<textarea class="adm-textarea" name="_note" rows="3">{data.presets.note}</textarea>
					</label>
				{/if}

				<div class="adm-rows">
					<table class="adm-table">
						<thead>
							<tr>
								<th>Name</th>
								<th>Label</th>
								<th>Pack</th>
								<th>Pack deps</th>
								<th>Adapter</th>
								<th>Engine</th>
								<th>Crawler</th>
								<th class="adm-end"></th>
							</tr>
						</thead>
						<tbody>
							{#each data.presets.rows as r (r.key)}
								<tr>
									<td class="adm-money">{r.key}</td>
									<td><input class="adm-input" name="label:{r.key}" value={r.label} /></td>
									<td><input class="adm-input" name="pack:{r.key}" value={r.pack} /></td>
									<td>
										<input
											class="adm-input"
											name="deps:{r.key}"
											value={r.deps}
											placeholder="weakest first"
										/>
									</td>
									<td><input class="adm-input" name="adapter:{r.key}" value={r.adapter} /></td>
									<td><input class="adm-input" name="engine:{r.key}" value={r.engine} /></td>
									<td>
										<input
											class="adm-input"
											name="crawler:{r.key}"
											value={r.crawler}
											list="crawlers"
											placeholder="known:host"
										/>
									</td>
									<td class="adm-end">
										<button
											class="btn btn--ghost btn--sm"
											type="submit"
											formaction="?/dropPreset"
											name="key"
											value={r.key}
											title="drop this preset"
										>
											drop
										</button>
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
				<datalist id="crawlers">
					{#each data.crawlerOptions as c (c)}<option value={c}></option>{/each}
				</datalist>

				<div class="wrap" style="margin-top:var(--space)">
					<button class="btn btn--primary" type="submit">Save presets</button>
					<button class="btn btn--ghost" type="submit" formaction="?/addPreset">Add preset</button>
				</div>
			</form>
		{:else}
			<form method="POST" action="?/saveJson" use:enhance>
				<input type="hidden" name="id" value={data.file.id} />
				<div class="adm-field">
					<span class="adm-label">
						JSON
						{#if jsonError}<span style="color:var(--color-primary)">: {jsonError}</span>{/if}
					</span>
					<textarea
						class="adm-textarea adm-code"
						name="text"
						oninput={(e) => check(e.currentTarget.value)}
						spellcheck="false"
						aria-invalid={jsonError !== null}>{data.body}</textarea
					>
				</div>
				<div class="wrap" style="margin-top:var(--space)">
					<button class="btn btn--primary" type="submit" disabled={jsonError !== null}>Save</button>
					<span class="muted">
						Saved with a two-space indent and one trailing newline. A file that does not parse is
						refused.
					</span>
				</div>
			</form>
		{/if}
	</section>
</div>
