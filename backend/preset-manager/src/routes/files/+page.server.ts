//! The checkout's own preset files. Presets get a real editor; the registries
//! are edited as JSON in place, parsed on the way in and normalized on the way
//! out so a save can never write a broken file.

import { readdirSync, readFileSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { fail } from '@sveltejs/kit';
import { MANAGED, managedFiles, managedPath, repoRoot } from '#lib/files';
import { orderLike, str, writeJson } from '#lib/json';
import type { Actions, PageServerLoad } from './$types';

export interface PresetRow {
	key: string;
	label: string;
	pack: string;
	deps: string;
	adapter: string;
	engine: string;
	crawler: string;
}

function presets(text: string): { note: string; hasNote: boolean; rows: PresetRow[] } {
	const doc = JSON.parse(text) as Record<string, unknown>;
	const rows = Object.entries(doc)
		.filter(([k]) => !k.startsWith('_'))
		.map(([key, raw]) => {
			const p = (raw ?? {}) as Record<string, unknown>;
			const crawler = (p.crawler ?? {}) as { type?: string; file?: string };
			const deps = Array.isArray(p.pack_deps) ? p.pack_deps.filter((d) => typeof d === 'string') : [];
			return {
				key,
				label: str(p.label),
				pack: str(p.pack),
				deps: deps.join(', '),
				adapter: str(p.adapter),
				engine: str(p.engine),
				crawler: crawler.type ? `${crawler.type}:${crawler.file ?? ''}` : ''
			};
		});
	return { note: str(doc._note), hasNote: typeof doc._note === 'string', rows };
}

/** The crawler selections a preset may name, for the field's datalist. */
function crawlerOptions(): string[] {
	const root = repoRoot();
	const out = ['none', 'custom:'];
	try {
		const known = JSON.parse(readFileSync(path.join(root, 'crawlers', 'knownsites.json'), 'utf8')) as Record<
			string,
			unknown
		>;
		for (const host of Object.keys(known)) if (!host.startsWith('_')) out.push(`known:${host}`);
	} catch {
		/* the registry is optional: a shorter datalist is not an error */
	}
	try {
		for (const f of readdirSync(path.join(root, 'crawlers', 'examples'))) {
			if (f.endsWith('.lua')) out.push(`example:${f}`);
		}
	} catch {
		/* same */
	}
	return out;
}

export const load: PageServerLoad = async ({ url }) => {
	const files = await managedFiles();
	const id = url.searchParams.get('id') ?? files.find((f) => f.exists)?.id ?? MANAGED[0].id;
	const file = files.find((f) => f.id === id) ?? files[0];

	let body = '';
	try {
		body = await readFile(managedPath(file.id), 'utf8');
	} catch (e) {
		return { files, file, body: '', readError: (e as Error).message, presets: null, crawlerOptions: [] };
	}

	return {
		files,
		file,
		body,
		readError: null,
		presets: file.id === 'presets' ? presets(body) : null,
		crawlerOptions: file.id === 'presets' ? crawlerOptions() : []
	};
};

/** Write normalized JSON, or say why not. */
async function write(id: string, doc: unknown): Promise<void> {
	await writeJson(managedPath(id), doc);
}

function parse(form: FormData, field: string): unknown {
	const raw = str(form.get(field));
	try {
		return JSON.parse(raw);
	} catch (e) {
		throw new Error(`not JSON: ${(e as Error).message}`);
	}
}

export const actions: Actions = {
	saveJson: async ({ request }) => {
		const form = await request.formData();
		const id = str(form.get('id'));
		if (!MANAGED.some((f) => f.id === id)) return fail(400, { ok: false, msg: `unknown file: ${id}` });
		let doc: unknown;
		try {
			doc = parse(form, 'text');
		} catch (e) {
			return fail(400, { ok: false, msg: (e as Error).message });
		}
		try {
			await write(id, doc);
		} catch (e) {
			return fail(500, { ok: false, msg: (e as Error).message });
		}
		return { ok: true, msg: `saved ${id}` };
	},

	savePresets: async ({ request }) => {
		const form = await request.formData();
		const disk = JSON.parse(await readFile(managedPath('presets'), 'utf8')) as Record<string, unknown>;
		const keys = str(form.get('keys')).split('\n').filter(Boolean);
		const next: Record<string, unknown> = {};
		const note = form.get('_note');
		if (typeof note === 'string') next._note = note;

		for (const key of keys) {
			const prev = (disk[key] ?? {}) as Record<string, unknown>;
			const values: Record<string, unknown> = {
				label: str(form.get(`label:${key}`)),
				pack: str(form.get(`pack:${key}`)),
				adapter: str(form.get(`adapter:${key}`)),
				engine: str(form.get(`engine:${key}`))
			};
			const deps = str(form.get(`deps:${key}`))
				.split(/[\s,]+/)
				.filter(Boolean);
			if (deps.length) values.pack_deps = deps;
			const crawler = str(form.get(`crawler:${key}`));
			if (crawler && crawler !== 'none') {
				const [type, ...rest] = crawler.split(':');
				// `custom` names no file, and the reader treats absent and empty alike.
				const file = rest.join(':');
				values.crawler = file ? { type, file } : { type };
			}
			for (const [k, v] of Object.entries(prev)) if (!(k in values)) values[k] = v;
			next[key] = orderLike(prev, values);
		}

		try {
			await write('presets', next);
		} catch (e) {
			return fail(500, { ok: false, msg: (e as Error).message });
		}
		return { ok: true, msg: `saved ${keys.length} presets` };
	},

	addPreset: async ({ request }) => {
		const form = await request.formData();
		const disk = JSON.parse(await readFile(managedPath('presets'), 'utf8')) as Record<string, unknown>;
		let n = 1;
		while (`untitled-${n}` in disk) n++;
		const key = str(form.get('key')) || `untitled-${n}`;
		await write('presets', { ...disk, [key]: { label: '', pack: '', adapter: '', engine: '' } });
		return { ok: true, msg: `added ${key}` };
	},

	dropPreset: async ({ request }) => {
		const form = await request.formData();
		const key = str(form.get('key'));
		const disk = JSON.parse(await readFile(managedPath('presets'), 'utf8')) as Record<string, unknown>;
		if (!(key in disk)) return fail(400, { ok: false, msg: `no preset named ${key}` });
		delete disk[key];
		await write('presets', disk);
		return { ok: true, msg: `dropped ${key}` };
	}
};
