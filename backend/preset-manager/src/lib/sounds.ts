//! The clip pools a sounds pack is made of: three registries, their clips, and
//! the tree an edit lands in. That tree is the downloaded working copy when
//! there is one, and the checkout's own `assets/` when there is not — either
//! way the edit is local until a release is published.

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { copyFile, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import path from 'node:path';
import { promisify } from 'node:util';
import { repoRoot } from './files';
import { num, orderLike, readJson, str, writeJson } from './json';
import { workRoot } from './packs';

const exec = promisify(execFile);

export interface Pool {
	/** The layer's name, and its URL segment under the pack. */
	id: 'effects' | 'music' | 'injects';
	/** Registry filename, relative to `assets/`. */
	file: string;
	/** Where its clips live, relative to `assets/`. */
	dir: string;
	label: string;
}

/** The layer order the mixer uses, which is this order. */
export const POOLS: Pool[] = [
	{ id: 'effects', file: 'effect-pool.json', dir: 'effects', label: 'Effects' },
	{ id: 'music', file: 'music-pool.json', dir: 'music', label: 'Music' },
	{ id: 'injects', file: 'inject-pool.json', dir: 'injects', label: 'Injects' }
];

export type PoolId = Pool['id'];

export function poolById(id: string): Pool | null {
	return POOLS.find((p) => p.id === id) ?? null;
}

export interface Sound {
	key: string;
	tags: string[];
	files: string[];
	looped: boolean;
	/** Set only where the registry sets it: the inject layer. */
	dur_s?: number;
	mode?: string;
	hold?: number;
	level?: number;
}

export interface PoolDoc {
	pool: Pool;
	sounds: Sound[];
	/** Named takes that are not on disk yet: the pack's pending work. */
	missing: number;
	/** Which named takes are on disk, keyed by the path the registry uses. */
	present: Record<string, boolean>;
}

export interface Tree {
	abs: string;
	/** Where the registries and clips live: `<abs>/assets`, or the tree itself. */
	assets: string;
	rel: string;
	/** Where the edits land: a pack tree, a downloaded copy, or the checkout. */
	label: string;
	downloaded: boolean;
	/** `assets/_extends/<name>`: the tree `profile.sh pack --dep` releases. */
	authored: boolean;
	/** That tree came from a release rather than from this checkout, so it drifts. */
	dependency: boolean;
}

/** The dependencies `_extends.json` records as resolved from releases. */
function resolvedDeps(root: string): Set<string> {
	try {
		const doc = readFileSync(path.join(root, 'assets', '_extends.json'), 'utf8');
		const tree = (JSON.parse(doc) as { tree?: { name?: unknown }[] }).tree ?? [];
		return new Set(
			tree.map((t) => t.name).filter((n): n is string => typeof n === 'string' && n !== '')
		);
	} catch {
		/* no record: nothing has been resolved here, so nothing is a dependency */
		return new Set();
	}
}

/**
 * The tree to edit: the pack's own tree at `assets/_extends/<name>` when this
 * checkout holds one, else the newest downloaded working copy, else the
 * checkout's own `assets/`. A pack that lives under `_extends/` is released from
 * there — by `--dep`, or folded into whatever composition names it — so that is
 * where an edit has to land to travel. A composition has no such tree and is
 * edited in place.
 */
export function treeFor(kind: string, name: string): Tree {
	const root = repoRoot();
	if (kind === 'sounds') {
		const dir = path.join(root, 'assets', '_extends', name);
		if (existsSync(dir)) {
			const dependency = resolvedDeps(root).has(name);
			return {
				abs: dir,
				assets: dir,
				rel: path.join('assets', '_extends', name),
				label: dependency
					? `assets/_extends/${name}, dependency tree`
					: `assets/_extends/${name}, authored here`,
				downloaded: false,
				authored: !dependency,
				dependency
			};
		}
	}
	for (const version of workingVersions(kind, name)) {
		const dir = path.join(workRoot(), kind, name, version);
		if (existsSync(path.join(dir, '.fetch.json')) && existsSync(path.join(dir, 'assets'))) {
			return {
				abs: dir,
				assets: path.join(dir, 'assets'),
				rel: path.join('.bm', 'packs', kind, name, version),
				label: `${version}, downloaded copy`,
				downloaded: true,
				authored: false,
				dependency: false
			};
		}
	}
	return {
		abs: root,
		assets: path.join(root, 'assets'),
		rel: '.',
		label: 'this checkout',
		downloaded: false,
		authored: false,
		dependency: false
	};
}

function workingVersions(kind: string, name: string): string[] {
	try {
		return readdirSync(path.join(workRoot(), kind, name), { withFileTypes: true })
			.filter((d) => d.isDirectory())
			.map((d) => d.name)
			.sort()
			.reverse();
	} catch {
		return [];
	}
}

/** `<tree>/assets`, which is what both a release and the live pipeline read. */
export function assetsOf(tree: Tree): string {
	return tree.assets;
}

function soundsOf(doc: Record<string, unknown>): Sound[] {
	return Object.entries(doc)
		.filter(([key]) => !key.startsWith('_'))
		.map(([key, raw]) => {
			const entry = (raw ?? {}) as Record<string, unknown>;
			const out: Sound = {
				key,
				tags: Array.isArray(entry.tags) ? entry.tags.filter((t) => typeof t === 'string') : [],
				files: Array.isArray(entry.files) ? entry.files.filter((f) => typeof f === 'string') : [],
				looped: entry.looped !== false
			};
			if (typeof entry.dur_s === 'number') out.dur_s = entry.dur_s;
			if (typeof entry.mode === 'string') out.mode = entry.mode;
			if (typeof entry.hold === 'number') out.hold = entry.hold;
			if (typeof entry.level === 'number') out.level = entry.level;
			return out;
		})
		.sort((a, b) => (a.key < b.key ? -1 : 1));
}

export async function readPools(tree: Tree): Promise<PoolDoc[]> {
	const assets = assetsOf(tree);
	return Promise.all(
		POOLS.map(async (pool) => {
			let sounds: Sound[] = [];
			try {
				sounds = soundsOf(await readJson(path.join(assets, pool.file)));
			} catch {
				/* a registry the pack does not ship is an empty layer, not an error */
			}
			const present: Record<string, boolean> = {};
			let missing = 0;
			for (const sound of sounds) {
				for (const take of sound.files) {
					present[take] = clipFile(tree, take) !== null;
					if (!present[take]) missing += 1;
				}
			}
			return { pool, sounds, missing, present };
		})
	);
}

/** A registry key, which is also a filename-safe token in every message. */
export const KEY = /^[a-z0-9][a-z0-9._-]*$/;

export function checkKey(name: string, what = 'sound'): string {
	if (!KEY.test(name)) throw new Error(`not a ${what} name: ${name}`);
	return name;
}

export interface SoundEdit {
	/** Null when the caller did not send the field, which leaves the tags alone. */
	tags: string | null;
	/** A take to append, already on disk; null when this edit adds none. */
	add: string | null;
	mode: string;
	hold: string;
	dur_s: string;
	level: string;
	/** Null leaves the flag alone, so a form that says nothing about it is safe. */
	looped: boolean | null;
}

/** Write one sound back, keeping the registry's key order and unknown fields. */
export async function saveSound(
	tree: Tree,
	pool: Pool,
	name: string,
	edit: SoundEdit
): Promise<void> {
	checkKey(name);
	const file = path.join(assetsOf(tree), pool.file);
	const doc = await readJson(file);
	const prev = (doc[name] ?? {}) as Record<string, unknown>;
	const values: Record<string, unknown> = {
		tags:
			edit.tags === null
				? prev.tags
				: edit.tags
						.split(/[\s,]+/)
						.map((t) => t.trim())
						.filter(Boolean)
						.filter((t, i, all) => all.indexOf(t) === i),
		mode: edit.mode || undefined,
		hold: num(edit.hold),
		level: num(edit.level)
	};
	// A moment's `dur_s` only grows: it clamps the family's holds and tails, so a
	// short take must not shorten what the longer ones already needed.
	if (pool.id === 'injects') {
		const given = [num(edit.dur_s), typeof prev.dur_s === 'number' ? prev.dur_s : undefined];
		const known = given.filter((n): n is number => typeof n === 'number' && n > 0);
		values.dur_s = known.length ? Math.max(...known) : undefined;
	}
	// The takes are only touched when one is added: a form that carries no file
	// leaves the list alone rather than emptying it.
	if (edit.add !== null) {
		const files = Array.isArray(prev.files) ? prev.files.filter((f) => typeof f === 'string') : [];
		values.files = [...files, edit.add];
	}
	// `looped` defaults to true, so it is written only when it says something:
	// when the entry already spelled it out, or when it is false.
	if (edit.looped === false || (edit.looped === true && 'looped' in prev)) values.looped = edit.looped;
	if (values.tags === undefined) delete values.tags;
	for (const [key, value] of Object.entries(values)) if (value === undefined) delete values[key];
	for (const [key, value] of Object.entries(prev)) if (!(key in values)) values[key] = value;

	doc[name] = orderLike(prev, values);
	await writeJson(file, doc);
}

/** Drop one take from a sound. The clip stays on disk; the registry stops naming it. */
export async function dropTake(tree: Tree, pool: Pool, name: string, rel: string): Promise<void> {
	checkKey(name);
	const file = path.join(assetsOf(tree), pool.file);
	const doc = await readJson(file);
	const prev = (doc[name] ?? {}) as Record<string, unknown>;
	const files = Array.isArray(prev.files) ? prev.files : [];
	const kept = files.filter((f) => f !== rel);
	if (kept.length === files.length) throw new Error(`${rel} is not a take of ${name}`);
	if (kept.length === 0) throw new Error(`${name} would be left with no takes`);
	doc[name] = { ...prev, files: kept };
	await writeJson(file, doc);
}

export async function addSound(tree: Tree, pool: Pool, name: string): Promise<void> {
	checkKey(name);
	const file = path.join(assetsOf(tree), pool.file);
	const doc = await readJson(file).catch(() => ({}) as Record<string, unknown>);
	if (name in doc) throw new Error(`${name} is already in ${pool.file}`);
	doc[name] = { tags: [], files: [] };
	await writeJson(file, doc);
}

export async function dropSound(tree: Tree, pool: Pool, name: string): Promise<void> {
	checkKey(name);
	const file = path.join(assetsOf(tree), pool.file);
	const doc = await readJson(file);
	delete doc[name];
	await writeJson(file, doc);
}

/**
 * Rename one registry key, keeping its position and its identity. The old key
 * becomes the entry's `id` when it has none, so the mix's pick and the merge's
 * design stamp do not move; the take files keep their names, because the
 * registry is what the mix reads.
 */
export async function renameSound(
	tree: Tree,
	pool: Pool,
	from: string,
	to: string
): Promise<void> {
	checkKey(from);
	checkKey(to);
	if (from === to) return;
	const file = path.join(assetsOf(tree), pool.file);
	const doc = await readJson(file);
	if (!(from in doc)) throw new Error(`${from} is not in ${pool.file}`);
	if (to in doc) throw new Error(`${to} is already in ${pool.file}`);
	const entry = { ...((doc[from] ?? {}) as Record<string, unknown>) };
	if (typeof entry.id !== 'string' || entry.id === '') entry.id = from;
	const next: Record<string, unknown> = {};
	for (const [key, value] of Object.entries(doc)) {
		next[key === from ? to : key] = key === from ? entry : value;
	}
	await writeJson(file, next);
}

/** A clip is served only from inside the tree's `assets/`, never by climbing out. */
export function clipFile(tree: Tree, rel: string): string | null {
	const assets = path.resolve(assetsOf(tree));
	const abs = path.resolve(assets, rel);
	if (abs !== assets && !abs.startsWith(assets + path.sep)) return null;
	return existsSync(abs) && statSync(abs).isFile() ? abs : null;
}

export const AUDIO: Record<string, string> = {
	'.mp3': 'audio/mpeg',
	'.wav': 'audio/wav',
	'.ogg': 'audio/ogg',
	'.oga': 'audio/ogg',
	'.opus': 'audio/ogg',
	'.flac': 'audio/flac',
	'.m4a': 'audio/mp4',
	'.aac': 'audio/aac'
};

/** The rung a layer's clips are normalized to, or null where the layer owns its
 * own spec. Music is that case: `add-music.sh` renders it as a crossfaded loop,
 * so a music take is placed exactly as it arrives. */
const RUNG: Record<PoolId, string | null> = { effects: '-26', injects: '-20', music: null };

/** The measured length: what an inject's `dur_s` has to hold, rather than what
 * the form said. */
async function durationOf(file: string): Promise<number | null> {
	try {
		const { stdout } = await exec('ffprobe', [
			'-v',
			'error',
			'-show_entries',
			'format=duration',
			'-of',
			'csv=p=0',
			file
		]);
		const seconds = Number(stdout.trim());
		return Number.isFinite(seconds) && seconds > 0 ? Math.round(seconds * 100) / 100 : null;
	} catch {
		return null;
	}
}

export interface Placed {
	/** The take path the registry names. */
	rel: string;
	/** Its measured length, or null when ffprobe could not read it. */
	dur: number | null;
}

/**
 * Put an uploaded clip into the pack the way the pipeline would: through
 * `tools/normalize-audio.sh`, which downmixes to mono, resamples to 48 kHz,
 * trims both ends and sets the layer's rung.
 *
 * Writing the bytes straight into the pool — what this used to do — shipped
 * whatever the browser sent: stereo, untrimmed, and at whatever loudness the
 * generator felt like. A `level` in the scene map is gain over a *known* source,
 * so a clip that skipped the rung quietly breaks every rule that reaches it.
 *
 * Staging lives outside the tree, because anything under `assets/` ships.
 */
export async function placeUpload(
	tree: Tree,
	pool: Pool,
	take: string,
	bytes: Uint8Array,
	original: string
): Promise<Placed> {
	const ext = path.extname(original).toLowerCase();
	if (!(ext in AUDIO)) throw new Error(`not an audio file name: ${original}`);
	if (take.includes('/') || take.includes('\\') || take.startsWith('.')) {
		throw new Error(`not a take name: ${take}`);
	}
	const rel = `${pool.dir}/${take}.mp3`;
	const dest = path.join(assetsOf(tree), rel);
	if (existsSync(dest)) {
		throw new Error(`${rel} is already there, drop the take instead of writing over it`);
	}

	const rung = RUNG[pool.id];
	if (rung === null) {
		await mkdir(path.dirname(dest), { recursive: true });
		await writeFile(dest, bytes);
		return { rel, dur: await durationOf(dest) };
	}

	const staging = path.join(repoRoot(), 'refs', 'temp', 'preset-manager');
	await mkdir(staging, { recursive: true });
	const stage = await mkdtemp(path.join(staging, 'upload-'));
	try {
		const src = path.join(stage, 'src');
		const out = path.join(stage, 'out');
		await mkdir(src, { recursive: true });
		await mkdir(out, { recursive: true });
		await writeFile(path.join(src, take + ext), bytes);
		await exec('bash', [path.join(repoRoot(), 'tools', 'normalize-audio.sh'), src, out], {
			env: { ...process.env, I_TARGET: rung }
		});
		const normalized = path.join(out, take + '.mp3');
		if (!existsSync(normalized)) {
			throw new Error(`normalize-audio.sh produced nothing for ${original}`);
		}
		await mkdir(path.dirname(dest), { recursive: true });
		await copyFile(normalized, dest);
	} finally {
		await rm(stage, { recursive: true, force: true }).catch(() => {});
	}
	return { rel, dur: await durationOf(dest) };
}
