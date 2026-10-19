//! One pack: what the bucket holds for it, what is down here, and the writes a
//! person actually makes — edit a voice preset, build a bundle, publish.

import { existsSync } from 'node:fs';
import path from 'node:path';
import { fail, redirect } from '@sveltejs/kit';
import { repoRoot } from '#lib/files';
import { authorPacks, download, findPack, type Pack } from '#lib/packs';
import { buildBundle, bundles, publish } from '#lib/release';
import { moods, renameMood, type Mood } from '#lib/palette';
import { declared, historyPath, readHistory } from '#lib/migrations';
import { POOLS, treeFor } from '#lib/sounds';
import {
	createPack,
	dropPack,
	packTree,
	publishVoicePack,
	readPackedPreset,
	savePackedPreset,
	takeNames
} from '#lib/voice-pack';
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

	// One preset is one pack, so a voices pack that is only down here — made but
	// not published yet — is a pack in its own right, not a missing bucket entry.
	const voiceTree = kind === 'voices' ? packTree(name) : null;
	if (!error && !pack && !voiceTree) error = `${kind}/${name} is not in the bucket`;
	const packed = voiceTree ? readPackedPreset(voiceTree, name) : { preset: null, error: null };
	// Every clone a pad could play, pack copy first — the datalist and the pads
	// both read it, so a named slot knows whether it has anything to hear.
	const takes = voiceTree ? takeNames(voiceTree) : [];

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
		preferred: local.find((rel) => rel.endsWith(`/${name}.tar.zst`)) ?? local[0] ?? '',
		voiceTree: voiceTree ? path.relative(repoRoot(), voiceTree) : null,
		voicePreset: packed.preset,
		voiceError: packed.error,
		voiceTakes: takes
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

	saveVoice: async ({ request, params }) => {
		const form = await request.formData();
		const name = params.name ?? '';
		const tree = packTree(name);
		if (!tree) {
			return fail(400, { ok: false, what: 'save preset', log: 'start or download the pack first' });
		}
		const result = savePackedPreset(tree, name, {
			label: String(form.get('label') ?? ''),
			category: String(form.get('category') ?? '')
				.split(/[\s,]+/)
				.filter(Boolean),
			// A row may name a clone; an empty name is numbered `unnamed-N`.
			voices: String(form.get('voices') ?? '[]')
		});
		if (!result.ok) return fail(400, { ok: false, what: 'save preset', log: result.msg });
		return { ok: true, what: 'save preset', log: result.msg };
	},

	createVoice: async ({ request }) => {
		const form = await request.formData();
		const name = String(form.get('name') ?? '')
			.trim()
			.toLowerCase();
		const result = createPack(name, String(form.get('label') ?? ''));
		if (!result.ok) return fail(400, { ok: false, what: 'new pack', log: result.msg });
		throw redirect(303, `/packs/voices/${encodeURIComponent(name)}`);
	},

	dropVoice: async ({ params }) => {
		const name = params.name ?? '';
		const tree = packTree(name);
		if (!tree) {
			return fail(400, { ok: false, what: 'delete pack', log: 'no working copy here' });
		}
		dropPack(tree, name);
		throw redirect(303, '/packs?kind=voices');
	},

	publishVoices: async ({ request, params }) => {
		const form = await request.formData();
		const result = publishVoicePack(
			params.name ?? '',
			String(form.get('version') ?? 'auto').trim() || 'auto'
		);
		if (!result.ok) return fail(400, { ok: false, what: 'publish voices', log: result.log });
		return { ok: true, what: 'publish voices', log: result.log };
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
