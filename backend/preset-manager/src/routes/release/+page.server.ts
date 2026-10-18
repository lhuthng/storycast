//! Building a pack release and publishing it: the same two steps the runbook
//! spells out, in the order it spells them out.

import { fail } from '@sveltejs/kit';
import { today } from '#lib/format';
import { buildModels as buildEngineBundle, engineInfos, verifyModels } from '#lib/models';
import { authorPacks, KINDS } from '#lib/packs';
import { buildBundle, bundles, publish } from '#lib/release';
import type { Actions, PageServerLoad } from './$types';

/** Where a bundle usually belongs, so the form starts on the right values. */
function guess(rel: string): { kind: string; name: string } {
	const base = rel.split('/').pop() ?? '';
	const name = base.replace(/\.(tar\.zst|tar\.gz|tar\.zstd)$/, '');
	if (rel.startsWith('engines/')) {
		const engine = rel.split('/')[1] ?? '';
		return { kind: 'models', name: engine };
	}
	if (rel.startsWith('profiles/adapter/')) return { kind: 'profiles', name };
	if (rel.startsWith('profiles/pack/')) return { kind: 'sounds', name };
	return { kind: '', name };
}

export const load: PageServerLoad = ({ url }) => {
	const list = bundles();
	const bundle = url.searchParams.get('bundle') ?? list[0]?.rel ?? '';
	const hinted = guess(bundle);
	const authors = authorPacks();
	const buildName = authors.find((a) => !a.bundled)?.name ?? authors[0]?.name ?? '';
	const engineList = engineInfos();
	return {
		authors,
		bundles: list,
		bundle,
		kind: url.searchParams.get('kind') ?? hinted.kind,
		name: url.searchParams.get('name') ?? hinted.name,
		// A build names a pack under assets/_extends/, which is a different thing
		// from the name an archive publishes under.
		buildName,
		engines: engineList,
		engine: url.searchParams.get('engine') ?? engineList[0]?.engine ?? 'vieneu',
		version: today(),
		kinds: KINDS
	};
};

export const actions: Actions = {
	build: async ({ request }) => {
		const form = await request.formData();
		const result = buildBundle(
			String(form.get('name') ?? ''),
			form.get('dep') === 'on',
			form.get('force') === 'on'
		);
		if (!result.ok) return fail(400, { ok: false, what: 'build', log: result.log });
		return { ok: true, what: 'build', log: result.log };
	},

	buildModels: async ({ request }) => {
		const form = await request.formData();
		const result = buildEngineBundle(
			String(form.get('engine') ?? ''),
			Number(form.get('level') ?? 3)
		);
		if (!result.ok) return fail(400, { ok: false, what: 'build models', log: result.log });
		return { ok: true, what: 'build models', log: result.log };
	},

	verifyModels: async ({ request }) => {
		const form = await request.formData();
		const result = verifyModels(String(form.get('engine') ?? ''));
		if (!result.ok) return fail(400, { ok: false, what: 'verify models', log: result.log });
		return { ok: true, what: 'verify models', log: result.log };
	},

	publish: async ({ request }) => {
		const form = await request.formData();
		const result = publish({
			kind: String(form.get('kind') ?? ''),
			name: String(form.get('name') ?? ''),
			version: String(form.get('version') ?? ''),
			bundle: String(form.get('bundle') ?? '')
		});
		if (!result.ok) return fail(400, { ok: false, what: 'publish', log: result.log });
		return { ok: true, what: 'publish', log: result.log };
	}
};
