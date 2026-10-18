<script lang="ts">
	import { enhance } from '$app/forms';
	import type { PageProps } from './$types';

	let { data, form }: PageProps = $props();

	const clip = (file: string) => `/packs/${data.kind}/${data.name}/clip/${file}`;
	const layer = (id: string) => `/packs/${data.kind}/${data.name}/${id}`;
	const current = $derived(data.current);
	const isInject = $derived(data.pool.id === 'injects');

	// Rows open independently: the panel is where a sound's tags and takes live,
	// and comparing two of them is a normal thing to want. The old page-wide
	// "Details" switch survives as Expand all, which is the same affordance for
	// someone editing the whole layer.
	let open = $state<Set<string>>(new Set());
	const isOpen = (key: string) => open.has(key);
	function toggle(key: string) {
		const next = new Set(open);
		if (next.has(key)) next.delete(key);
		else next.add(key);
		open = next;
	}
	const allOpen = $derived(current.sounds.length > 0 && open.size >= current.sounds.length);
	function toggleAll() {
		open = allOpen ? new Set() : new Set(current.sounds.map((s) => s.key));
	}

	// A click belongs to the row only when it is not a control's. Without this,
	// picking a type or hitting a pad would also collapse the panel being read.
	const CONTROL = 'button, input, select, textarea, label, a';
	function rowClick(e: MouseEvent, key: string) {
		if ((e.target as HTMLElement).closest(CONTROL)) return;
		toggle(key);
	}
	const panelId = (key: string) => `rack-${encodeURIComponent(key)}`;

	let playing = $state<string | null>(null);
	let audio: HTMLAudioElement | null = null;

	function play(file: string) {
		if (playing === file) {
			audio?.pause();
			audio = null;
			playing = null;
			return;
		}
		audio?.pause();
		const next = new Audio(clip(file));
		next.onended = () => {
			if (playing === file) playing = null;
		};
		next.play();
		audio = next;
		playing = file;
	}

	// Leaving the page must not leave a clip running.
	$effect(() => () => audio?.pause());

	// Every control commits on change: a field the form does not carry is left
	// alone by the action, so a one-control form is a complete edit.
	const commit = (e: Event) => {
		const el = e.currentTarget as HTMLInputElement | HTMLSelectElement;
		el.form?.requestSubmit();
	};

	// Destructive, but recoverable — the registry entry goes and the audio stays
	// on disk — so the confirmation says exactly that rather than warning darkly.
	const confirmDrop = (e: MouseEvent, key: string) => {
		if (!confirm(`Delete “${key}”? The registry entry goes; its take files stay on disk.`)) {
			e.preventDefault();
		}
	};

	// A take is the one edit that moves the roll, so the confirmation names both
	// what survives (the clip) and what does not (the chapter audio that re-merges).
	const confirmDropTake = (e: MouseEvent, key: string, file: string, left: number) => {
		const takes = left === 1 ? '1 take would be left' : `${left} takes would be left`;
		const msg = `Drop ${file} from ${key}? ${takes}, and the roll shifts — chapters reaching this sound re-merge. The clip stays on disk, but the + pad will not take it back.`;
		if (!confirm(msg)) e.preventDefault();
	};

	const ICONS: Record<string, { d: string[]; fill?: boolean }> = {
		play: { d: ['M9 6.2 L18 12 L9 17.8 Z'], fill: true },
		pause: { d: ['M9.5 6.5v11', 'M14.5 6.5v11'] },
		plus: { d: ['M12 6.5v11', 'M6.5 12h11'] },
		x: { d: ['M7 7l10 10', 'M17 7 7 17'] },
		chevron: { d: ['M9.5 6.5 15 12l-5.5 5.5'] },
		trash: {
			d: [
				'M4.5 7h15',
				'M9.5 7V5.4a.9.9 0 0 1 .9-.9h3.2a.9.9 0 0 1 .9.9V7',
				'M6.6 7l.8 11.7a1.4 1.4 0 0 0 1.4 1.3h6.4a1.4 1.4 0 0 0 1.4-1.3L17.4 7',
				'M10.2 10.6v5.6',
				'M13.8 10.6v5.6'
			]
		}
	};
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

<svelte:head><title>{data.name} · {data.pool.label}</title></svelte:head>

<a class="adm-back" href="/packs/{data.kind}/{data.name}">← {data.kind}/{data.name}</a>

<div class="wrap" style="margin-bottom:var(--space)">
	<h1 class="adm-h1" style="margin:0">{data.pool.label}</h1>
	<span class="chip" class:chip--live={data.tree.downloaded}>{data.tree.label}</span>
	{#if current.sounds.length}
		<button
			class="btn btn--ghost btn--sm"
			type="button"
			aria-pressed={allOpen}
			onclick={toggleAll}
			style="margin-left:auto">{allOpen ? 'Collapse all' : 'Expand all'}</button
		>
	{/if}
</div>

{#if data.tree.dependency}
	<p class="muted" style="margin:0 0 var(--space)">
		This is the dependency tree unpacked from its release, and it is what the composition
		folds in — so an edit here ships. It also drifts: <span class="adm-money"
			>profile update</span
		> refuses to replace a tree that was edited here without <span class="adm-money">--force</span
		>.
	</p>
{/if}

<nav class="layer-tabs" aria-label="Layers">
	{#each data.layers as l (l.pool.id)}
		<a href={layer(l.pool.id)} aria-current={l.pool.id === data.pool.id ? 'page' : undefined}>
			{l.pool.label}
			<span>{l.sounds.length}</span>
			{#if l.missing}<em>{l.missing} missing</em>{/if}
		</a>
	{/each}
</nav>

{#if form && !form.ok}
	<div class="banner banner--err">{form.msg ?? 'that edit was refused'}</div>
{/if}

{#if !data.hasAssets}
	<div class="banner banner--err">
		No <span class="adm-money">assets/</span> in this tree.
	</div>
{:else if current.sounds.length === 0}
	<p class="empty">Nothing in this layer yet.</p>
{:else}
	<div class="rack">
		<!-- The header is the legend: a row's first cell is a key, and the word
		     beside it says what that key is *for*, which is the question a wall of
		     `air-swing`, `tree-hit`, `water-spell` raises. -->
		<div class="rack-head">
			<span class="rack-col">
				<b>Sound</b>
				<span>the key a scene rule or the model writes</span>
			</span>
			<span class="rack-col">
				<b>{isInject ? 'Mode' : 'Play'}</b>
				<span>{isInject ? 'how takes stack: hit, overlap or trail' : 'looped, or one-shot'}</span>
			</span>
			<span class="rack-col">
				<b>Takes</b>
				<span>the audio files; one is drawn per roll</span>
			</span>
			<span class="rack-col rack-col--end"><b>&nbsp;</b></span>
		</div>

		{#each current.sounds as s (s.key)}
			<form
				class="rack-row"
				class:rack-row--open={isOpen(s.key)}
				method="POST"
				action="?/saveSound"
				use:enhance
				enctype="multipart/form-data"
				onclick={(e) => rowClick(e, s.key)}
			>
				<input type="hidden" name="sound" value={s.key} />

				<div class="rack-cell">
					<span class="rack-name" id={`name-${panelId(s.key)}`}>{s.key}</span>
					{#if !isOpen(s.key)}
						<span class="rack-tags-preview">
							{#each s.tags.slice(0, 4) as t (t)}<span class="chip chip--tag">{t}</span>{/each}
							{#if s.tags.length > 4}<span class="muted">+{s.tags.length - 4}</span>{/if}
							{#if s.tags.length === 0}<span class="muted">no tags</span>{/if}
						</span>
					{/if}
				</div>

				<label class="rack-type">
					<span class="sr-only">{isInject ? 'Mode' : 'Playback'}</span>
					<select
						class="adm-select"
						name={isInject ? `mode:${s.key}` : `looped:${s.key}`}
						onchange={commit}
					>
						{#if isInject}
							{#if !s.mode}<option value="" selected>unset</option>{/if}
							{#each ['hit', 'overlap', 'trail'] as m (m)}
								<option value={m} selected={s.mode === m}>{m}</option>
							{/each}
						{:else}
							<option value="on" selected={s.looped}>looped</option>
							<option value="" selected={!s.looped}>one-shot</option>
						{/if}
					</select>
				</label>

				<div class="pads">
					{#each s.files as f (f)}
						{#if current.present[f]}
							<button
								type="button"
								class="pad"
								class:pad--on={playing === f}
								aria-pressed={playing === f}
								aria-label={playing === f ? 'Pause this take' : 'Play this take'}
								title={playing === f ? 'Pause' : 'Play'}
								onclick={() => play(f)}
							>
								{@render icon(playing === f ? 'pause' : 'play')}
							</button>
						{:else}
							<span class="pad pad--missing" title="the registry names it, but it is not on disk">?</span>
						{/if}
					{/each}

					<label class="pad pad--add" title="Add a take">
						<input
							class="sr-only"
							type="file"
							name={`upload:${s.key}`}
							accept="audio/*"
							onchange={commit}
						/>
						{@render icon('plus')}
					</label>
				</div>

				<button
					class="rack-toggle"
					type="button"
					aria-expanded={isOpen(s.key)}
					aria-controls={panelId(s.key)}
					aria-label={isOpen(s.key) ? `Collapse ${s.key}` : `Expand ${s.key}`}
					title={isOpen(s.key) ? 'Collapse' : 'Expand'}
					onclick={() => toggle(s.key)}
				>
					{@render icon('chevron')}
				</button>

				{#if isOpen(s.key)}
					<div class="rack-panel" id={panelId(s.key)}>
						<section class="rack-section">
							<h3>Tags</h3>
							<p class="rack-hint">
								What the model writes to reach this sound. A row with no tags is only reachable by
								name.
							</p>
							{#if s.tags.length}
								<div class="tag-cloud">
									{#each s.tags as t (t)}<span class="chip chip--tag">{t}</span>{/each}
								</div>
							{/if}
							<label class="adm-field" style="max-width:34rem">
								<span class="adm-label">Tags, comma separated</span>
								<input
									class="adm-input"
									name={`tags:${s.key}`}
									value={s.tags.join(', ')}
									onchange={commit}
								/>
							</label>
						</section>

						<section class="rack-section">
							<h3>Mix</h3>
							<div class="rack-fields">
								<label class="adm-field">
									<span class="adm-label">Level</span>
									<input
										class="adm-input"
										name={`level:${s.key}`}
										value={s.level ?? ''}
										inputmode="decimal"
										onchange={commit}
									/>
								</label>
								{#if isInject}
									<label class="adm-field">
										<span class="adm-label">Hold (s)</span>
										<input
											class="adm-input"
											name={`hold:${s.key}`}
											value={s.hold ?? ''}
											inputmode="decimal"
											onchange={commit}
										/>
									</label>
									<label class="adm-field">
										<span class="adm-label">Longest take (s)</span>
										<input
											class="adm-input"
											name={`dur_s:${s.key}`}
											value={s.dur_s ?? ''}
											inputmode="decimal"
											onchange={commit}
										/>
									</label>
								{/if}
							</div>
						</section>

						<section class="rack-section">
							<h3>Takes</h3>
							<p class="rack-hint">
								The roll picks one of these; dropping a take changes how many there are.
							</p>
							<ul class="take-list">
								{#each s.files as f (f)}
									<li class="take-item" class:take-item--missing={!current.present[f]}>
										<span class="adm-money">{f}</span>
										{#if current.present[f]}
											{#if s.files.length > 1}
												<button
													class="take-x"
													type="submit"
													formaction="?/dropTake"
													name="file"
													value={f}
													aria-label="Drop {f}"
													title="Drop this take"
													onclick={(e) => confirmDropTake(e, s.key, f, s.files.length - 1)}
													>{@render icon('x')}</button
												>
											{:else}
												<span class="muted" title="the pack gate refuses a sound with no takes"
													>only take</span
												>
											{/if}
										{:else}
											<span class="chip chip--warn">not on disk</span>
										{/if}
									</li>
								{/each}
							</ul>
						</section>

						<section class="rack-section">
							<h3>Rename</h3>
							<div class="wrap" style="align-items:end">
								<label class="adm-field" style="max-width:22rem">
									<span class="adm-label">Name</span>
									<input class="adm-input adm-code" name={`rename:${s.key}`} value={s.key} />
								</label>
								<button class="btn btn--ghost btn--sm" type="submit" formaction="?/renameSound">
									Rename
								</button>
							</div>
						</section>

						<section class="rack-section rack-section--danger">
							<h3>Delete</h3>
							<p class="rack-hint">
								Removes the entry from <span class="adm-money">{data.pool.file}</span>. The take files
								stay on disk, so this is undoable by adding the key back.
							</p>
							<button
								class="btn btn--sm btn--danger"
								type="submit"
								formaction="?/dropSound"
								onclick={(e) => confirmDrop(e, s.key)}
							>
								{@render icon('trash')} Delete {s.key}
							</button>
						</section>
					</div>
				{/if}
			</form>
		{/each}
	</div>

	<form method="POST" action="?/addSound" use:enhance class="wrap" style="margin-top:var(--space)">
		<label class="adm-field" style="flex:1;max-width:22rem">
			<span class="adm-label">New {data.pool.label.toLowerCase()} sound</span>
			<input class="adm-input adm-code" name="new" placeholder="cave-drip" required />
		</label>
		<button class="btn btn--ghost btn--sm" type="submit" style="align-self:end">Add</button>
	</form>
{/if}
