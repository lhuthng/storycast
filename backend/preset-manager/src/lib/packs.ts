//! Published packs: what the bucket holds, and the working copy a download
//! leaves under `.bm/packs/<kind>/<name>/<version>/`.
//!
//! Only a version `latest.json` still pins can be downloaded: an older version
//! has no hash to verify against, and a silent unverified download is worse
//! than a refusal.

import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { repoRoot } from './files';
import { config, fetchToFile, getJson, hashFile, listObjects, type BucketObject } from './r2';

/** Order the packs are listed in: the bundle you fetch once, then the art. */
export const KINDS = ['models', 'sounds', 'voices', 'profiles'];

export interface PackFile {
	file: string;
	key: string;
	sha256: string;
	bytes: number;
}

export interface Latest {
	version: string;
	manifestHash: string;
	files: PackFile[];
}

export interface PackVersion {
	version: string;
	bytes: number;
	files: number;
	modified: string;
	downloaded: boolean;
}

export interface Pack {
	kind: string;
	name: string;
	versions: PackVersion[];
	latest: Latest | null;
	/** Why `latest.json` could not be used, when it could not. */
	latestError: string | null;
	/** A working copy down here that the bucket has not got yet. */
	local?: boolean;
}

/** The one preset a voices pack holds, inside its working copy. */
export const PRESET_FILE = 'preset.json';

/** Where a voices pack of `name` keeps its preset under `dir`. */
export function presetPath(dir: string, name: string): string {
	return path.join(dir, 'voices', name, PRESET_FILE);
}

export interface Inventory {
	packs: Pack[];
	error: string | null;
	objects: number;
}

/** A bucket listing and one HTTP round trip per pack is the slowest thing here,
 *  and it does not change between two page loads. Five minutes, or a write of
 *  our own, or an explicit refresh. */
const CACHE_MS = 5 * 60_000;
let cached: { at: number; value: Inventory } | null = null;
let inflight: Promise<Inventory> | null = null;

/** After a write of ours: a download changes what `downloaded` says, a publish
 *  changes the bucket. */
export function forgetInventory(): void {
	cached = null;
}

/** The checkout-local working copies: ignored by git, safe to delete. */
export function workRoot(): string {
	return path.join(repoRoot(), '.bm', 'packs');
}

export function workRel(kind: string, name: string, version: string): string {
	return path.join('.bm', 'packs', kind, name, version);
}

export function workDir(kind: string, name: string, version: string): string {
	return path.join(workRoot(), kind, name, version);
}

const KEY = /^preset\/([^/]+)\/([^/]+)\/([^/]+)\/(.+)$/;

interface Parsed {
	kind: string;
	name: string;
	version: string;
	file: string;
	bytes: number;
	modified: string;
}

function parseKeys(objects: BucketObject[]): Parsed[] {
	const out: Parsed[] = [];
	for (const o of objects) {
		const m = KEY.exec(o.key);
		if (!m) continue;
		const [, kind, name, version, file] = m;
		if (file === 'latest.json') continue;
		out.push({ kind, name, version, file, bytes: o.bytes, modified: o.modified });
	}
	return out;
}

/** `latest.json` for one pack: the only document that pins hashes. */
export async function latestOf(kind: string, name: string): Promise<Latest | null> {
	const cfg = config();
	if (!cfg) return null;
	const url = `${cfg.base}/preset/${kind}/${name}/latest.json`;
	const doc = await getJson<{
		version?: string;
		manifest_hash?: string;
		files?: Record<string, { key?: string; sha256?: string; bytes?: number }>;
	}>(url);
	if (!doc) return null;
	const files: PackFile[] = Object.entries(doc.files ?? {}).map(([file, entry]) => ({
		file,
		key: entry.key ?? '',
		sha256: entry.sha256 ?? '',
		bytes: entry.bytes ?? 0
	}));
	return { version: doc.version ?? '', manifestHash: doc.manifest_hash ?? '', files };
}

/** Every pack in the bucket, newest version first. Never throws: the page says so. */
export async function inventory(fresh = false): Promise<Inventory> {
	if (!fresh && cached && Date.now() - cached.at < CACHE_MS) return withLocal(cached.value);
	if (!fresh && inflight) return withLocal(await inflight);
	const run = loadInventory();
	inflight = run;
	const value = await run;
	inflight = null;
	// A failure is not cached: the next ask is the retry.
	if (!value.error) cached = { at: Date.now(), value };
	return withLocal(value);
}

/**
 * Packs this checkout holds that the bucket has not got yet. One preset is one
 * pack, so a voices pack exists from the moment it is started, and reading the
 * working copies is cheap enough to do on every ask rather than cache.
 */
function withLocal(inv: Inventory): Inventory {
	if (inv.error) return inv;
	const known = new Set(inv.packs.map((p) => `${p.kind}/${p.name}`));
	const extra: Pack[] = [];
	for (const name of localVoicesPacks()) {
		if (known.has(`voices/${name}`)) continue;
		extra.push({ kind: 'voices', name, versions: [], latest: null, latestError: null, local: true });
	}
	if (!extra.length) return inv;
	const packs = [...inv.packs, ...extra].sort((a, b) => {
		const k = KINDS.indexOf(a.kind) - KINDS.indexOf(b.kind);
		if (k !== 0) return k;
		return a.name < b.name ? -1 : 1;
	});
	return { ...inv, packs };
}

function localVoicesPacks(): string[] {
	const base = path.join(workRoot(), 'voices');
	let names: string[];
	try {
		names = readdirSync(base, { withFileTypes: true })
			.filter((d) => d.isDirectory())
			.map((d) => d.name);
	} catch {
		return [];
	}
	return names.filter((name) => localVersions(name).length > 0).sort();
}

/** Versions of a working copy that hold the pack's own preset. */
function localVersions(name: string): string[] {
	let versions: string[];
	try {
		versions = readdirSync(path.join(workRoot(), 'voices', name), { withFileTypes: true })
			.filter((d) => d.isDirectory())
			.map((d) => d.name);
	} catch {
		return [];
	}
	return versions.filter((v) => existsSync(presetPath(workDir('voices', name, v), name)));
}

async function loadInventory(): Promise<Inventory> {
	let objects: BucketObject[];
	try {
		objects = listObjects();
	} catch (e) {
		return { packs: [], error: (e as Error).message, objects: 0 };
	}

	const grouped = new Map<string, Map<string, { bytes: number; files: number; modified: string }>>();
	for (const p of parseKeys(objects)) {
		const key = `${p.kind}/${p.name}`;
		const versions = grouped.get(key) ?? new Map();
		const v = versions.get(p.version) ?? { bytes: 0, files: 0, modified: '' };
		v.bytes += p.bytes;
		v.files += 1;
		if (p.modified > v.modified) v.modified = p.modified;
		versions.set(p.version, v);
		grouped.set(key, versions);
	}

	const packs = [...grouped.entries()].map(([key, versions]) => {
		const [kind, name] = key.split('/');
		return {
			kind,
			name,
			versions: [...versions.entries()]
				.map(([version, v]) => ({
					version,
					...v,
					downloaded: existsSync(path.join(workDir(kind, name, version), '.fetch.json'))
				}))
				.sort((a, b) => (a.version < b.version ? 1 : -1))
		};
	});

	packs.sort((a, b) => {
		const k = KINDS.indexOf(a.kind) - KINDS.indexOf(b.kind);
		if (k !== 0) return k;
		return a.name < b.name ? -1 : 1;
	});

	const withLatest = await Promise.all(
		packs.map(async (p) => {
			try {
				return { ...p, latest: await latestOf(p.kind, p.name), latestError: null };
			} catch (e) {
				return { ...p, latest: null, latestError: (e as Error).message };
			}
		})
	);

	return { packs: withLatest, error: null, objects: objects.length };
}

export async function findPack(kind: string, name: string, fresh = false): Promise<Pack | null> {
	const inv = await inventory(fresh);
	if (inv.error) throw new Error(inv.error);
	return inv.packs.find((p) => p.kind === kind && p.name === name) ?? null;
}

export interface DownloadResult {
	ok: boolean;
	log: string;
	dir: string | null;
}

const tarEnv = { ...process.env };

function tar(argv: string[], cwd: string): { ok: boolean; out: string } {
	const r = spawnSync('tar', argv, { cwd, encoding: 'utf8', timeout: 600_000, env: tarEnv });
	if (r.error) return { ok: false, out: `tar cannot run: ${r.error.message}` };
	const out = `${(r.stdout ?? '').trim()}\n${(r.stderr ?? '').trim()}`.trim();
	return { ok: r.status === 0, out };
}

/** A pack archive is ours, but an absolute or climbing path in it is still refused. */
function listArchive(archive: string, dir: string): { ok: boolean; out: string; entries: string[] } {
	const r = tar(['--use-compress-program=unzstd', '-tf', archive], dir);
	const entries = r.out.split('\n').map((l) => l.trim()).filter(Boolean);
	if (!r.ok) return { ok: false, out: r.out, entries };
	const bad = entries.find(
		(e) => e.startsWith('/') || e.split('/').includes('..') || /^[A-Za-z]:/.test(e)
	);
	if (bad) return { ok: false, out: `refused a climbing path in the archive: ${bad}`, entries };
	return { ok: true, out: r.out, entries };
}

function lines(...parts: string[]): string {
	return parts.filter(Boolean).join('\n');
}

/**
 * Fetch the version `latest.json` points at, verify every hash, and unpack it
 * into the working copy. Idempotent: a second run keeps what is already there.
 */
export async function download(kind: string, name: string, force = false): Promise<DownloadResult> {
	const cfg = config();
	if (!cfg) return { ok: false, log: 'no R2 credentials', dir: null };
	const latest = await latestOf(kind, name);
	if (!latest || !latest.version) return { ok: false, log: 'no latest.json for this pack', dir: null };

	const dir = workDir(kind, name, latest.version);
	const receipt = path.join(dir, '.fetch.json');
	if (existsSync(receipt) && !force) {
		const kept = JSON.parse(readFileSync(receipt, 'utf8')) as { fetchedAt?: string; sha256?: string };
		return {
			ok: true,
			log: lines(
				`already downloaded: ${workRel(kind, name, latest.version)}`,
				`fetched ${kept.fetchedAt ?? '?'}`,
				'tick Force to fetch it again'
			),
			dir
		};
	}

	try {
		rmSync(dir, { recursive: true, force: true });
		mkdirSync(dir, { recursive: true });
	} catch (e) {
		return { ok: false, log: `cannot prepare ${dir}: ${(e as Error).message}`, dir: null };
	}

	const log: string[] = [];
	for (const entry of latest.files) {
		const url = `${cfg.base}/${entry.key}`;
		const archive = path.join(dir, path.basename(entry.file));
		try {
			fetchToFile(url, archive);
		} catch (e) {
			return { ok: false, log: lines(...log, `download failed: ${(e as Error).message}`), dir: null };
		}
		const bytes = statSync(archive).size;
		log.push(`fetched ${entry.key} (${bytes} bytes)`);

		if (entry.sha256) {
			const got = hashFile(archive);
			if (got !== entry.sha256) {
				rmSync(archive, { force: true });
				return {
					ok: false,
					log: lines(...log, `sha256 mismatch: expected ${entry.sha256}, got ${got}`),
					dir: null
				};
			}
			log.push(`sha256 ok (${got.slice(0, 16)}…)`);
		}

		const listed = listArchive(archive, dir);
		if (!listed.ok) {
			return { ok: false, log: lines(...log, listed.out), dir: null };
		}
		const unpacked = tar(['--use-compress-program=unzstd', '-xf', archive], dir);
		if (!unpacked.ok) {
			return { ok: false, log: lines(...log, `tar failed: ${unpacked.out}`), dir: null };
		}
		log.push(`unpacked ${listed.entries.length} entries`);
		rmSync(archive, { force: true });
	}

	const manifest = readManifest(dir);
	if (manifest) {
		const deps = manifest.deps.length ? manifest.deps.join(', ') : 'none (a root pack)';
		log.push(`manifest.json: ${manifest.files} files, deps: ${deps}`);
	} else {
		log.push('no manifest.json in the archive');
	}

	writeFileSync(
		receipt,
		`${JSON.stringify(
			{
				kind,
				name,
				version: latest.version,
				manifestHash: latest.manifestHash,
				files: latest.files,
				fetchedAt: new Date().toISOString()
			},
			null,
			2
		)}\n`
	);
	log.push(`working copy: ${workRel(kind, name, latest.version)}`);
	forgetInventory();
	return { ok: true, log: lines(...log), dir };
}

export interface Manifest {
	name: string;
	version: string;
	deps: string[];
	files: number;
}

/** The archive's own manifest, when the unpacked tree carries one. */
export function readManifest(dir: string): Manifest | null {
	try {
		const doc = JSON.parse(readFileSync(path.join(dir, 'manifest.json'), 'utf8')) as {
			name?: string;
			version?: string;
			deps?: string[];
			files?: Record<string, unknown>;
		};
		return {
			name: doc.name ?? '',
			version: doc.version ?? '',
			deps: doc.deps ?? [],
			files: Object.keys(doc.files ?? {}).length
		};
	} catch {
		return null;
	}
}

export interface Working {
	kind: string;
	name: string;
	version: string;
	rel: string;
	bytes: number;
	files: number;
	entries: string[];
	updated: string;
	deps: string[];
}

function sizeTree(dir: string): { files: number; bytes: number; entries: string[] } {
	const out = { files: 0, bytes: 0, entries: [] as string[] };
	const walk = (d: string, top: boolean) => {
		let items;
		try {
			items = readdirSync(d, { withFileTypes: true });
		} catch {
			return;
		}
		for (const item of items) {
			const full = path.join(d, item.name);
			if (item.isDirectory()) {
				if (top) out.entries.push(`${item.name}/`);
				walk(full, false);
			} else if (item.isFile()) {
				// The receipt is this tool's bookkeeping, not part of the pack.
				if (top && !item.name.startsWith('.')) out.entries.push(item.name);
				try {
					out.bytes += statSync(full).size;
					out.files += 1;
				} catch {
					/* a file that vanished mid-walk is not worth failing over */
				}
			}
		}
	};
	walk(dir, true);
	return out;
}

export function working(kind: string, name: string, version: string): Working | null {
	const dir = workDir(kind, name, version);
	if (!existsSync(path.join(dir, '.fetch.json'))) return null;
	const size = sizeTree(dir);
	const manifest = readManifest(dir);
	let updated = '';
	try {
		updated = statSync(dir).mtime.toISOString();
	} catch {
		/* keep the empty stamp */
	}
	return {
		kind,
		name,
		version,
		rel: workRel(kind, name, version),
		bytes: size.bytes,
		files: size.files,
		entries: size.entries.sort(),
		updated,
		deps: manifest?.deps ?? []
	};
}

export interface AuthorPack {
	name: string;
	rel: string;
	files: number;
	bytes: number;
	/** What its own `pack.json` names, weakest first. Empty for a root pack. */
	deps: string[];
	/** A bundle for it exists under profiles/pack/, so it can be published. */
	bundled: boolean;
}

/** The packs authored in this checkout: `assets/_extends/<name>/`. */
export function authorPacks(): AuthorPack[] {
	const root = repoRoot();
	const dir = path.join(root, 'assets', '_extends');
	let names: string[];
	try {
		names = readdirSync(dir, { withFileTypes: true })
			.filter((d) => d.isDirectory())
			.map((d) => d.name);
	} catch {
		return [];
	}
	return names
		.map((name) => {
			const size = sizeTree(path.join(dir, name));
			let deps: string[] = [];
			try {
				const doc = JSON.parse(readFileSync(path.join(dir, name, 'pack.json'), 'utf8')) as {
					deps?: unknown;
				};
				if (Array.isArray(doc.deps)) deps = doc.deps.filter((d): d is string => typeof d === 'string');
			} catch {
				/* a root pack has no pack.json, or names no deps */
			}
			return {
				name,
				rel: path.join('assets', '_extends', name),
				files: size.files,
				bytes: size.bytes,
				deps,
				bundled: existsSync(path.join(root, 'profiles', 'pack', `${name}.tar.zst`))
			};
		})
		.sort((a, b) => (a.name < b.name ? -1 : 1));
}

/** Every working copy on this machine, newest pack first. */
export function workingCopies(): Working[] {
	const out: Working[] = [];
	let kinds: string[];
	try {
		kinds = readdirSync(workRoot(), { withFileTypes: true })
			.filter((d) => d.isDirectory())
			.map((d) => d.name);
	} catch {
		return out;
	}
	for (const kind of kinds) {
		let names: string[];
		try {
			names = readdirSync(path.join(workRoot(), kind), { withFileTypes: true })
				.filter((d) => d.isDirectory())
				.map((d) => d.name);
		} catch {
			continue;
		}
		for (const name of names) {
			let versions: string[];
			try {
				versions = readdirSync(path.join(workRoot(), kind, name), { withFileTypes: true })
					.filter((d) => d.isDirectory())
					.map((d) => d.name);
			} catch {
				continue;
			}
			for (const version of versions) {
				const w = working(kind, name, version);
				if (w) out.push(w);
			}
		}
	}
	return out;
}
