//! One layer of a sounds pack: its registry, its takes, and every write to it.
//! The layer is the URL, so a form can never write a different one. Every form
//! here is a partial one — a control commits on change, and a field the form
//! does not carry is left alone.

import { existsSync } from 'node:fs';
import path from 'node:path';
import { error, fail } from '@sveltejs/kit';
import { str } from '#lib/json';
import {
	addSound,
	assetsOf,
	AUDIO,
	dropSound,
	dropTake,
	placeUpload,
	poolById,
	readPools,
	renameSound,
	saveSound,
	treeFor,
	type Pool
} from '#lib/sounds';
import type { Actions, PageServerLoad } from './$types';

const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

function poolOr404(id: string): Pool {
	const pool = poolById(id);
	if (!pool) throw error(404, `no such layer: ${id}`);
	return pool;
}

export const load: PageServerLoad = async ({ params }) => {
	const pool = poolOr404(params.pool);
	const tree = treeFor(params.kind, params.name);
	// All three layers: the current one to edit, the others for the counts beside them.
	const layers = await readPools(tree);
	return {
		kind: params.kind,
		name: params.name,
		tree,
		hasAssets: existsSync(assetsOf(tree)),
		pool,
		layers,
		current: layers.find((l) => l.pool.id === pool.id) ?? { pool, sounds: [], missing: 0, present: {} }
	};
};

export const actions: Actions = {
	saveSound: async ({ request, params }) => {
		const pool = poolOr404(params.pool);
		const form = await request.formData();
		const sound = str(form.get('sound'));

		const tree = treeFor(params.kind, params.name);
		let add: string | null = null;
		/** Measured from the placed clip, so the form cannot claim a length. */
		let measured: number | null = null;

		const upload = form.get(`upload:${sound}`);
		if (upload instanceof File && upload.size > 0) {
			const name = path.basename(upload.name);
			const ext = path.extname(name).toLowerCase();
			if (!NAME.test(name) || !(ext in AUDIO)) {
				return fail(400, { ok: false, msg: `not an audio file name: ${upload.name}` });
			}
			try {
				const placed = await placeUpload(
					tree,
					pool,
					path.basename(name, ext),
					new Uint8Array(await upload.arrayBuffer()),
					name
				);
				add = placed.rel;
				measured = placed.dur;
			} catch (e) {
				return fail(400, { ok: false, msg: (e as Error).message });
			}
		}

		const tags = form.get(`tags:${sound}`);
		const looped = form.get(`looped:${sound}`);
		try {
			await saveSound(tree, pool, sound, {
				tags: tags === null ? null : str(tags),
				add,
				mode: str(form.get(`mode:${sound}`)),
				hold: str(form.get(`hold:${sound}`)),
				dur_s: measured === null ? str(form.get(`dur_s:${sound}`)) : String(measured),
				level: str(form.get(`level:${sound}`)),
				looped: looped === null ? null : str(looped) === 'on'
			});
		} catch (e) {
			return fail(400, { ok: false, msg: (e as Error).message });
		}
		return { ok: true };
	},

	dropTake: async ({ request, params }) => {
		const pool = poolOr404(params.pool);
		const form = await request.formData();
		try {
			await dropTake(
				treeFor(params.kind, params.name),
				pool,
				str(form.get('sound')),
				str(form.get('file'))
			);
		} catch (e) {
			return fail(400, { ok: false, msg: (e as Error).message });
		}
		return { ok: true };
	},

	addSound: async ({ request, params }) => {
		const pool = poolOr404(params.pool);
		const name = str((await request.formData()).get('new'));
		try {
			await addSound(treeFor(params.kind, params.name), pool, name);
		} catch (e) {
			return fail(400, { ok: false, msg: (e as Error).message });
		}
		return { ok: true };
	},

	renameSound: async ({ request, params }) => {
		const pool = poolOr404(params.pool);
		const form = await request.formData();
		const sound = str(form.get('sound'));
		const to = str(form.get(`rename:${sound}`)).trim();
		try {
			await renameSound(treeFor(params.kind, params.name), pool, sound, to);
		} catch (e) {
			return fail(400, { ok: false, msg: (e as Error).message });
		}
		return { ok: true };
	},

	dropSound: async ({ request, params }) => {
		const pool = poolOr404(params.pool);
		const name = str((await request.formData()).get('sound'));
		try {
			await dropSound(treeFor(params.kind, params.name), pool, name);
		} catch (e) {
			return fail(400, { ok: false, msg: (e as Error).message });
		}
		return { ok: true };
	}
};
