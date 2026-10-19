<script lang="ts">
	import { untrack } from 'svelte';
	import { enhance } from '$app/forms';
	import { today, when } from '#lib/format';
	import type { PageProps } from './$types';

	let { data, form }: PageProps = $props();

	// Only the version latest.json pins can be verified, so only it is offered a download.
	const latestVersion = $derived(data.pack?.latest?.version ?? null);
	// Handed over as plain data: `#lib/sounds` reads the disk, so a component
	// cannot import it without dragging node:fs into the browser build.
	const layers = $derived(data.layers);

	// Voice presets: tabs across the top, one slot per row below. A slot may name
	// a clone — then its take plays here — and it may just as well stay unnamed,
	// which the file keeps as `unnamed-N` in order.
	type Row = { voice: string; tags: string[]; draft: string };
	type Preset = { label: string; category: string[]; voices: { voice: string; tags: string[] }[] };

	const rowsFrom = (preset: Preset | null): Row[] =>
		(preset?.voices ?? []).map((v) => ({ voice: v.voice, tags: [...v.tags], draft: '' }));

	function posted(list: Row[]): string {
		return JSON.stringify(list.map((r) => ({ voice: r.voice, tags: r.tags })));
	}

	let rows = $state<Row[]>(untrack(() => rowsFrom(data.voicePreset as Preset | null)));
	let shown = untrack(() => data.voiceTree ?? '');
	let baseline = untrack(() => posted(rows));
	$effect(() => {
		const tree = data.voiceTree ?? '';
		if (tree !== shown) {
			shown = tree;
			rows = rowsFrom(data.voicePreset as Preset | null);
			baseline = posted(rows);
		}
	});
	$effect(() => {
		if (form?.ok) baseline = untrack(() => posted(rows));
	});

	const rowsJson = () => posted(rows);
	const dirty = $derived(rowsJson() !== baseline);
	const slotCount = $derived(data.voicePreset?.voices.length ?? 0);
	const hasTake = (voice: string) => !!voice && data.voiceTakes.includes(voice);
	// `unnamed-N` is the file's placeholder for a role, not a name that failed
	// to resolve, so it is never flagged as missing a take.
	const namesClone = (voice: string) => !!voice && !voice.startsWith('unnamed-');

	// One take at a time: pressing another pad, saving nothing, or leaving the
	// page stops whatever is playing.
	let playing = $state<number | null>(null);
	let audio: HTMLAudioElement | null = null;
	// The clone picker is only attached to the field being typed in: a name that
	// needs no suggestion reads as plain text, with no browser chevron.
	let editing = $state<number | null>(null);
	function play(i: number, voice: string) {
		if (!hasTake(voice)) return;
		if (playing === i) {
			audio?.pause();
			audio = null;
			playing = null;
			return;
		}
		audio?.pause();
		const next = new Audio(`/packs/${data.kind}/${data.name}/take/${encodeURIComponent(voice)}`);
		const stop = () => {
			if (playing === i) playing = null;
		};
		next.onended = stop;
		next.onerror = stop;
		next.play();
		audio = next;
		playing = i;
	}
	function silence() {
		audio?.pause();
		audio = null;
		playing = null;
	}
	$effect(() => () => audio?.pause());

	const ICONS: Record<string, { d: string[]; fill?: boolean }> = {
		play: { d: ['M9 6.2 L18 12 L9 17.8 Z'], fill: true },
		pause: { d: ['M9.5 6.5v11', 'M14.5 6.5v11'] }
	};

	// The tag aliases match `bm-core`'s, so a chip shows the word the file keeps.
	const ALIAS: Record<string, string> = { elderly: 'old', powerful: 'strong' };
	const tagOf = (raw: string) => {
		const tag = raw.trim().toLowerCase().replace(/[\s,]+/g, '-');
		return ALIAS[tag] ?? tag;
	};
	/** The box takes a list: `old, male` is two tags, not one with a comma. */
	function addTag(r: Row) {
		for (const part of r.draft.split(',')) {
			const tag = tagOf(part);
			if (tag && !r.tags.includes(tag)) r.tags.push(tag);
		}
		r.draft = '';
	}
	const removeTag = (r: Row, i: number) => r.tags.splice(i, 1);
	const addSlot = () => rows.push({ voice: '', tags: [], draft: '' });
	function dropSlot(i: number) {
		silence();
		// The focused field is leaving, and a shifted index would otherwise keep
		// the picker attached to a row nobody is typing in.
		if (editing !== null && editing >= i) editing = null;
		rows.splice(i, 1);
	}

	// Force is the destructive half: the fetch deletes the working copy before
	// unpacking, and that copy is where the sounds editor writes.
	function confirmFetch(e: MouseEvent, version: string, downloaded: boolean) {
		if (!downloaded) return;
		const dir = `.bm/packs/${data.kind}/${data.name}/${version}`;
		const msg = `Fetch ${version} again? ${dir} is deleted first, so any sound edit made in it is lost.`;
		if (!confirm(msg)) e.preventDefault();
	}
</script>

{#snippet icon(name: string)}
	<svg
		viewBox="0 0 24 24"
		fill={ICONS[name].fill ? 'currentColor' : 'none'}
		stroke="currentColor"
		stroke-width="1.6"
		stroke-linecap="round"
		stroke-linejoin="round"
		aria-hidden="true"
	>
		{#each ICONS[name].d as d (d)}<path {d} />{/each}
	</svg>
{/snippet}

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
{:else}		<div class="adm-grid">
			{#if data.kind === 'voices'}
				<section class="card">
					<div class="wrap">
						<h2 style="margin:0">Presets</h2>
						<span class="muted" style="margin-left:auto">
							<span class="adm-money">voices/{data.name}/preset.json</span> · {slotCount} slot(s)
						</span>
					</div>

					{#if !data.voiceTree}
						<div class="banner">
							No preset down here: download a published version of this pack first. A new pack is
							started from <a href="/packs?kind=voices">the packs list</a> with an empty preset.
						</div>
					{:else}
						{#if data.voiceError}
							<div class="banner banner--err">{data.voiceError}</div>
						{/if}
						{#if data.pack?.local}
							<div class="banner">
								<span class="adm-money">voices/{data.name}</span> is not in the bucket yet — publish it
								below to create the pack.
							</div>
						{/if}

						{#if data.voicePreset}
							<form method="POST" action="?/saveVoice" use:enhance>
								<input type="hidden" name="voices" value={rowsJson()} />

								<div class="wrap" style="align-items:flex-end">
									<label class="adm-field">
										<span class="adm-label">Label</span>
										<input class="adm-input" name="label" value={data.voicePreset.label} />
									</label>
									<label class="adm-field">
										<span class="adm-label">Categories</span>
										<input
											class="adm-input"
											name="category"
											value={data.voicePreset.category.join(', ')}
											placeholder="none"
										/>
									</label>
									<span class="chip">preset.json</span>
									{#if dirty}<span class="chip chip--warn">unsaved</span>{/if}
									<button
										class="btn btn--danger btn--sm"
										type="submit"
										formaction="?/dropVoice"
										onclick={(e) => {
											if (!confirm(`Delete the working copy of voices/${data.name}?`)) {
												e.preventDefault();
											}
										}}
									>
										Delete pack
									</button>
									<button
										class="btn btn--primary btn--sm"
										type="submit"
										disabled={!dirty}
										style="margin-left:auto">Save preset</button
									>
								</div>

								<p class="adm-sub" style="margin-top:var(--space)">
									A slot holds what to cast: tags describe it, and a name picks one clone. Leave the
									name empty and the slot stays <span class="adm-money">unnamed-N</span>, which a publish
									ships as a role with no take.
								</p>

								<datalist id="takes">
									{#each data.voiceTakes as v (v)}<option value={v}></option>{/each}
								</datalist>

								<div class="slots">
									{#each rows as r, i (i)}
										<div class="slot-row">
											<span class="slot-n">#{i + 1}</span>
											<input
												class="adm-input adm-code"
												list={editing === i ? 'takes' : undefined}
												bind:value={r.voice}
												placeholder="unnamed"
												title="The clone this slot casts as. Left empty the slot is unnamed and carries no take."
												onfocus={() => (editing = i)}
												onblur={() => (editing = null)}
											/>
											<span class="rack-tags">
												{#each r.tags as t, j (t)}
													<span class="chip chip--tag">
														{t}<button
															type="button"
															class="chip-x"
															aria-label={`remove ${t}`}
															onclick={() => removeTag(r, j)}>×</button
														>
													</span>
												{/each}
												<input
													class="adm-input"
													style="max-width:10rem"
													bind:value={r.draft}
													placeholder="old, male"
													onkeydown={(e) => {
														if (e.key !== 'Enter') return;
														e.preventDefault();
														addTag(r);
													}}
													onblur={() => addTag(r)}
												/>
												{#if namesClone(r.voice) && !hasTake(r.voice)}
													<span
														class="chip chip--warn"
														title="no clone by that name has a take on disk; a publish refuses it">
														no take
													</span>
												{/if}
											</span>
											<span class="pads">
												<button
													type="button"
													class="pad"
													class:pad--on={playing === i}
													disabled={!hasTake(r.voice)}
													aria-label={playing === i ? 'Pause this take' : 'Play this take'}
													title={hasTake(r.voice)
														? 'Play this slot\u2019s take'
														: 'name a built clone to hear it'}
													onclick={() => play(i, r.voice)}
												>
													{@render icon(playing === i ? 'pause' : 'play')}
												</button>
												<button
													type="button"
													class="icon-x"
													aria-label={`remove slot ${i + 1}`}
													title="Remove this slot"
													onclick={() => dropSlot(i)}>×</button
												>
											</span>
										</div>
									{/each}
									{#if rows.length === 0}
										<p class="empty">No slots yet — add one for each voice this preset needs.</p>
									{/if}
								</div>

								<div class="wrap" style="margin-top:var(--tight)">
									<button class="btn btn--ghost btn--sm" type="button" onclick={addSlot}>
										Add slot
									</button>
									<span class="muted">{rows.length} slot(s)</span>
								</div>
							</form>
						{/if}

					{/if}
				</section>
			{/if}

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
			{#if data.kind === 'voices'}
				<h2>Publish the pack</h2>
				<p class="adm-sub">
					Runs <span class="adm-money">tools/preset-publish.sh</span> on the working copy: this pack's
					preset and the clones it names, so the take and <span class="adm-money">voice.json</span>
					of every slot that names a clone ride along. An <span class="adm-money">unnamed-N</span>
					slot ships as a role with no audio.
				</p>
				{#if !data.voiceTree}
					<div class="banner">Start the pack first; publishing packs the working copy.</div>
				{:else}
					<form method="POST" action="?/publishVoices" use:enhance>
						<div class="adm-fields">
							<label class="adm-field" style="max-width:12rem">
								<span class="adm-label">Version</span>
								<input class="adm-input" name="version" value={today()} />
							</label>
						</div>
						<button class="btn btn--primary" type="submit" style="margin-top:var(--space)">
							Publish to R2
						</button>
					</form>
				{/if}
			{:else}
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
			{/if}
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

