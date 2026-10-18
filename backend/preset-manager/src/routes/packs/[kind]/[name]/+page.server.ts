//! One pack: what the bucket holds for it, what is down here, and the two
//! writes a person actually makes — build a bundle, publish a new version.

import { existsSync } from 'node:fs';
import path from 'node:path';
import { fail } from '@sveltejs/kit';
import { repoRoot } from '#lib/files';
import { authorPacks, download, findPack, type Pack } from '#lib/packs';
import { buildBundle, bundles, publish } from '#lib/release';
import { moods, renameMood, type Mood } from '#lib/palette';
import { declared, historyPath, readHistory } from '#lib/migrations';
import { POOLS, treeFor } from '#lib/sounds';
import type { Actions, PageServerLoad } from './$types';

export const load: PageServerLoad = async ({ params, url }) => {
	const { kind, name } = params;
	const fresh = url.searchParams.has('refresh');
	const local: string[] = bundles().map((b) => b.rel);
	const author = authorPacks().find((a) => a.name === name) ?? null;
	const authorDir = path.join(repoRoot(), 'assets', '_extends', name);

	// The palette is read from the tree the sounds editor writes, so a rename
	// made here is a rename of what the next bundle will carry.
	let palette: Mood[] = [];
	let paletteError: string | null = null;
	if (kind === 'sounds') {
		try {
			palette = await moods(treeFor(kind, name));
		} catch (e) {
			paletteError = (e as Error).message;
		}
	}
	const history = await readHistory(name);

	let pack: Pack | null = null;
	let error: string | null = null;
	try {
		pack = await findPack(kind, name, fresh);
	} catch (e) {
		error = (e as Error).message;
	}
	if (!error && !pack) error = `${kind}/${name} is not in the bucket`;

	return {
		kind,
		name,
		pack,
		error,
		authorDirExists: existsSync(authorDir),
		author,
		// Where the sounds editor's writes land: the pack's authored tree when
		// this checkout holds one, else the newest copy down here, else the
		// checkout itself. `versions` is newest first.
		tree: kind === 'sounds' ? treeFor(kind, name) : null,
		// Plain data, because a component cannot reach this module: it pulls in
		// node:fs and the browser build has none.
		layers: kind === 'sounds' ? POOLS.map((p) => ({ id: p.id, label: p.label })) : [],
		moods: palette,
		paletteError,
		renames: declared(history),
		historyFile: path.relative(repoRoot(), historyPath(name)),
		bundles: local,
		preferred: local.find((rel) => rel.endsWith(`/${name}.tar.zst`)) ?? local[0] ?? ''
	};
};

export const actions: Actions = {
	renameMood: async ({ request, params }) => {
		const form = await request.formData();
		const from = String(form.get('mood') ?? '');
		const to = String(form.get(`rename:${from}`) ?? '').trim();
		try {
			await renameMood(treeFor('sounds', params.name ?? ''), params.name ?? '', from, to);
		} catch (e) {
			return fail(400, { ok: false, what: 'rename mood', log: (e as Error).message });
		}
		return {
			ok: true,
			what: 'rename mood',
			log: `${from} -> ${to}: the pack now declares it, so a workspace rewrites its scripts on update`
		};
	},

	download: async ({ request }) => {
		const form = await request.formData();
		const kind = String(form.get('kind') ?? '');
		const name = String(form.get('name') ?? '');
		const force = form.get('force') === 'on';
		const result = await download(kind, name, force);
		if (!result.ok) return fail(400, { ok: false, what: 'download', log: result.log });
		return { ok: true, what: 'download', log: result.log };
	},

	build: async ({ request }) => {
		const form = await request.formData();
		const name = String(form.get('name') ?? '');
		const result = buildBundle(
			name,
			form.get('dep') === 'on',
			form.get('force') === 'on'
		);
		if (!result.ok) return fail(400, { ok: false, what: 'build', log: result.log });
		return { ok: true, what: 'build', log: result.log };
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
