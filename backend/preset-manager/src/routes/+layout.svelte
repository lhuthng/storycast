<script lang="ts">
	// Bottom tabs on a phone, a persistent sidebar from 64rem, same items, same order.
	import { page } from '$app/state';
	import '../lib/admin.css';
	import type { LayoutProps } from './$types';

	let { children, data }: LayoutProps = $props();

	// 24px stroke icons, drawn here: they have no life outside this file.
	const ICONS: Record<string, string[]> = {
		overview: ['M4 10.5 12 4l8 6.5', 'M6 10.5V20h12v-9.5', 'M10 20v-5h4v5'],
		packs: ['M12 3 3.5 7.3v9.4L12 21l8.5-4.3V7.3z', 'M3.5 7.3 12 11.6l8.5-4.3', 'M12 11.6V21'],
		files: ['M6 3.5h7l5 5v12H6z', 'M13 3.5V9h5', 'M9 13h6', 'M9 16.5h6'],
		release: ['M12 16V4.5', 'M7.5 9 12 4.5 16.5 9', 'M5 19.5h14']
	};

	const SECTIONS = [
		{ href: '/', label: 'Overview', icon: 'overview' },
		{ href: '/packs', label: 'Packs', icon: 'packs' },
		{ href: '/files', label: 'Files', icon: 'files' },
		{ href: '/release', label: 'Release', icon: 'release' }
	];

	const path = $derived(page.url.pathname);
	const isCurrent = (href: string) =>
		href === '/' ? path === '/' : path === href || path.startsWith(`${href}/`);
</script>

{#snippet navItems(klass: string)}
	{#each SECTIONS as item (item.href)}
		<a class={klass} href={item.href} aria-current={isCurrent(item.href) ? 'page' : undefined}>
			<svg
				viewBox="0 0 24 24"
				fill="none"
				stroke="currentColor"
				stroke-width="1.6"
				stroke-linecap="round"
				stroke-linejoin="round"
				aria-hidden="true"
			>
				{#each ICONS[item.icon] as d (d)}<path {d} />{/each}
			</svg>
			{item.label}
		</a>
	{/each}
{/snippet}

<div class="adm-shell">
	<header class="adm-topbar">
		<a class="adm-wordmark" href="/">STORYCAST <span>presets</span></a>
		<span class="chip" class:chip--live={data.r2.listed} class:chip--warn={!data.r2.listed}>
			{data.r2.listed ? data.r2.bucket : 'no R2 credentials'}
		</span>
	</header>

	<nav class="adm-side" aria-label="Sections">
		<a class="adm-wordmark" href="/">STORYCAST <span>presets</span></a>
		<div style="display:grid;gap:var(--tight)">{@render navItems('adm-link')}</div>
		<span class="chip" class:chip--live={data.r2.listed} class:chip--warn={!data.r2.listed}>
			{data.r2.listed ? data.r2.bucket : 'no R2 credentials'}
		</span>
	</nav>

	<main class="adm-main">
		{@render children()}
	</main>

	<nav class="adm-tabbar" aria-label="Sections">
		{@render navItems('adm-tab')}
	</nav>
</div>
