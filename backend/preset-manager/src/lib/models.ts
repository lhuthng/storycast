//! The engine's bake as one transfer file: `engines/<engine>/models/models.tar.zst`.
//!
//! This was `tools/models.sh`. It lives here because releasing is this app's
//! job now, and because the three things the script did are three things the
//! manager already had a page for: pack a bundle, verify it, publish it. The
//! publish half was already here — `release.ts` hands any archive to
//! `tools/preset-publish.sh` — so only the packing and the verification moved.
//!
//! What did *not* move is the format. The bundle is still a tar of
//! `manifest.json` plus every file the manifest lists, zstd-compressed at the
//! top level with no `models/` prefix, because `bm-core`'s unpacker expects
//! exactly that and the manifest inside is what every box verifies against.
//! Tar and zstd stay external binaries for the same reason `profile.sh` keeps
//! them: they are as standard as ssh, and a JS re-implementation of either
//! would be a second definition of what the archive means.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
	closeSync,
	existsSync,
	mkdtempSync,
	openSync,
	readFileSync,
	readdirSync,
	readSync,
	rmSync,
	statSync,
	writeFileSync
} from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { repoRoot } from './files';
import type { Outcome } from './release';

/** An engine name lands in a path, so it is checked rather than trusted. */
const ENGINE = /^[A-Za-z0-9._-]+$/;
const TAIL_LINES = 30;

function tail(text: string): string {
	const lines = text.split('\n').filter((l) => l.trim() !== '');
	if (lines.length <= TAIL_LINES) return lines.join('\n');
	return ['…', ...lines.slice(-TAIL_LINES)].join('\n');
}

function transcript(stdout: string | null, stderr: string | null): string {
	return tail([(stdout ?? '').trim(), (stderr ?? '').trim()].filter(Boolean).join('\n'));
}

function short(hash: string): string {
	return hash.slice(0, 12);
}

export interface ModelsManifest {
	files: Record<string, { bytes?: number; sha256: string }>;
	_note?: string;
	[key: string]: unknown;
}

export interface EngineInfo {
	engine: string;
	/** `engines/<engine>/models`, relative to the checkout root. */
	rel: string;
	weights: number;
	/** Unpacked size the manifest claims, summed. */
	bytes: number;
	/** The bundle on disk, when one has been built. */
	bundle: { bytes: number; modified: string } | null;
	hash: string | null;
}

/** Every engine that has a `models/` tree, whether or not it is baked. */
export function engines(): string[] {
	const dir = path.join(repoRoot(), 'engines');
	if (!existsSync(dir)) return [];
	return readdirSync(dir, { withFileTypes: true })
		.filter((d) => d.isDirectory() && existsSync(path.join(dir, d.name, 'models', 'manifest.json')))
		.map((d) => d.name)
		.sort();
}

/** The manifest of one engine's bake, or a throw naming what is missing. */
export function manifest(engine: string): ModelsManifest {
	const p = manifestPath(engine);
	if (!existsSync(p)) throw new Error(`no bake manifest: ${p}`);
	return JSON.parse(readFileSync(p, 'utf8')) as ModelsManifest;
}

/**
 * The manifest hash: sha256 over sorted `name + NUL + sha256 + NUL` lines.
 *
 * The same rule `profile.sh` and `bm-core::artifact::manifest_hash` use, with
 * one level more of indirection because a models entry carries `bytes` beside
 * the hash. Both sides hash the *pairing*, never the JSON, so re-indenting a
 * manifest cannot rename a bundle.
 */
export function manifestHash(m: ModelsManifest): string {
	const h = createHash('sha256');
	for (const name of Object.keys(m.files).sort()) {
		h.update(name);
		h.update(Buffer.from([0]));
		h.update(m.files[name].sha256);
		h.update(Buffer.from([0]));
	}
	return h.digest('hex');
}

/**
 * The sha256 of a file, read in chunks.
 *
 * Not `readFileSync` + `hash.update`: a single weight here is several hundred
 * megabytes, and hashing 21 of them by slurping each one whole is a memory
 * spike for no reason.
 */
function sha256File(p: string): string {
	const h = createHash('sha256');
	const fd = openSync(p, 'r');
	try {
		const buf = Buffer.allocUnsafe(1 << 20);
		for (;;) {
			const n = readSync(fd, buf, 0, buf.length, null);
			if (n === 0) break;
			h.update(buf.subarray(0, n));
		}
	} finally {
		closeSync(fd);
	}
	return h.digest('hex');
}

/**
 * Every way the live tree disagrees with the manifest that describes it.
 *
 * Only what the manifest lists is examined. A stray file in the directory is
 * not a problem — `voices.json` lives there deliberately and is derived state
 * the bake never records — because the bundle is built from the manifest's
 * names, so an unlisted file cannot travel.
 */
export function checkBake(dir: string, m: ModelsManifest): string[] {
	const out: string[] = [];
	for (const name of Object.keys(m.files).sort()) {
		const p = path.join(dir, name);
		if (!existsSync(p)) {
			out.push(`  MISSING ${name}`);
			continue;
		}
		const got = sha256File(p);
		if (got !== m.files[name].sha256) {
			out.push(`  CHANGED ${name}\n    want ${m.files[name].sha256}\n    got  ${got}`);
		}
	}
	return out;
}

function engineDir(engine: string): string {
	if (!ENGINE.test(engine)) {
		throw new Error(
			`'${engine}' is not an engine name: letters, digits, dot, dash and underscore only`
		);
	}
	const dir = path.join(repoRoot(), 'engines', engine);
	if (!existsSync(dir)) {
		const known = existsSync(path.join(repoRoot(), 'engines'))
			? readdirSync(path.join(repoRoot(), 'engines')).join(' ')
			: '(none)';
		throw new Error(`no such engine: engines/${engine} — known: ${known}`);
	}
	return dir;
}

function modelsDir(engine: string): string {
	return path.join(engineDir(engine), 'models');
}

function manifestPath(engine: string): string {
	return path.join(modelsDir(engine), 'manifest.json');
}

/** Where a built bundle lives, relative to the checkout root. */
export function bundleRel(engine: string): string {
	return path.join('engines', engine, 'models', 'models.tar.zst');
}

/** One engine, as the release page shows it. */
export function engineInfo(engine: string): EngineInfo {
	const dir = modelsDir(engine);
	let m: ModelsManifest | null = null;
	let hash: string | null = null;
	try {
		m = manifest(engine);
		hash = manifestHash(m);
	} catch {
		// A manifest that will not parse is a bake problem, not a listing problem.
	}
	const bundlePath = path.join(dir, 'models.tar.zst');
	let bundle: EngineInfo['bundle'] = null;
	if (existsSync(bundlePath)) {
		const s = statSync(bundlePath);
		bundle = { bytes: s.size, modified: s.mtime.toISOString() };
	}
	const files = m ? Object.values(m.files) : [];
	return {
		engine,
		rel: path.relative(repoRoot(), dir),
		weights: files.length,
		bytes: files.reduce((n, f) => n + (f.bytes ?? 0), 0),
		bundle,
		hash
	};
}

/** Every engine with a bake, for the picker. */
export function engineInfos(): EngineInfo[] {
	return engines().map(engineInfo);
}

/**
 * The bundle: gate the bake, then tar the manifest and the weights into one
 * zstd stream.
 *
 * The gate is not a formality, but it is also not an interpreter's job. Every
 * file the manifest lists must be on disk with the sha256 it records — that is
 * the entire contract, because the bundle ships the manifest and exactly those
 * files, and a box verifies one against the other on arrival. A bake that
 * disagrees with its own record would therefore produce a bundle that fails
 * its own check 380 MB later, on someone else's box.
 *
 * `bake-models.py --check` tests the same thing, so running it here bought
 * nothing and cost a python3 requirement on a box that only packs.
 */
export function buildModels(engine: string, level = 3): Outcome {
	let dir: string;
	let m: ModelsManifest;
	try {
		dir = modelsDir(engine);
		m = manifest(engine);
	} catch (e) {
		return { ok: false, log: (e as Error).message };
	}
	if (!Number.isInteger(level) || level < 1 || level > 19) {
		return { ok: false, log: `bad level: ${level} (1–19)` };
	}
	for (const bin of ['tar', 'zstd']) {
		const probe = spawnSync('sh', ['-c', `command -v ${bin}`], { encoding: 'utf8' });
		if (probe.status !== 0) return { ok: false, log: `${bin} not on PATH` };
	}

	// The gate, in-process: every listed file present, with the recorded hash.
	const drift = checkBake(dir, m);
	if (drift.length) {
		// An engine that ships its own bake is the one to re-run; everything else
		// falls back to the one bake that exists.
		const own = path.join(engineDir(engine), 'bake.py');
		const bake = existsSync(own) ? own : path.join(repoRoot(), 'tools', 'bake-models.py');
		return {
			ok: false,
			log: [
				...drift,
				'refusing to pack: the bake does not match its manifest (above)',
				`re-bake with: python3 ${path.relative(repoRoot(), bake)} --out ${path.relative(repoRoot(), dir)}`
			].join('\n')
		};
	}

	// The manifest first, so a reader that stops early has already seen the
	// index. A file of names rather than an argv expansion: 21 is safe today,
	// and a bake with a thousand shards should not be a quoting accident.
	const members = ['manifest.json', ...Object.keys(m.files).sort()];
	const scratch = mkdtempSync(path.join(tmpdir(), 'bm-models-'));
	try {
		const listFile = path.join(scratch, 'members');
		writeFileSync(listFile, members.join('\n') + '\n');
		const bundle = path.join(dir, 'models.tar.zst');
		const tmp = `${bundle}.tmp`;
		// `-T` reads names relative to `-C`, so the archive holds `manifest.json`
		// and the weights at the top level — no `models/` prefix to strip on the
		// way out, which is what lets the fetch side untar straight into the
		// worker root.
		//
		// `COPYFILE_DISABLE=1` because macOS `tar` is libarchive, and for every
		// member carrying an extended attribute it writes a `._name` AppleDouble
		// sidecar into the archive. Its own `tar -t` hides them, so the packing
		// machine sees a clean bundle; a Linux box does not, and unpacks them as
		// junk files its manifest never listed.
		const packed = spawnSync(
			'bash',
			[
				'-c',
				'COPYFILE_DISABLE=1 tar -cf - -C "$1" -T "$2" | zstd -"$3" -q -o "$4"',
				'pack',
				dir,
				listFile,
				String(level),
				tmp
			],
			{ cwd: repoRoot(), encoding: 'utf8', timeout: 1_800_000 }
		);
		if (packed.status !== 0 || !existsSync(tmp)) {
			return { ok: false, log: transcript(packed.stdout, packed.stderr) || 'tar | zstd failed' };
		}
		const bytes = statSync(tmp).size;
		const renamed = spawnSync('mv', [tmp, bundle], { encoding: 'utf8' });
		if (renamed.status !== 0) {
			rmSync(tmp, { force: true });
			return { ok: false, log: transcript(renamed.stdout, renamed.stderr) };
		}
		const hash = manifestHash(m);
		return {
			ok: true,
			log: [
				`packed ${bundleRel(engine)}  (${(bytes / (1024 * 1024)).toFixed(0)} MiB at level ${level})`,
				`engine ${engine}`,
				// Members are the manifest plus every weight, so the weight count is
				// one less — said precisely because "22 files" reads as 22 weights.
				`files  ${members.length - 1} weights + manifest.json`,
				`hash   ${hash}`,
				`tag    models-v${short(hash)}`
			].join('\n')
		};
	} finally {
		rmSync(scratch, { recursive: true, force: true });
	}
}

/**
 * `models.sh verify`: unpack the bundle and hold it to the manifest inside it.
 *
 * Both directions are checked, because both are failures worth naming: a file
 * the manifest lists and the archive lacks would fail a box's own check, and a
 * file travelling that nobody listed is a bake record that is wrong — the box
 * would be verifying a set it was never told about.
 */
export function verifyModels(engine: string): Outcome {
	const bundle = path.join(modelsDir(engine), 'models.tar.zst');
	if (!existsSync(bundle)) {
		return { ok: false, log: `no bundle at ${bundleRel(engine)} (build it first)` };
	}
	const stage = mkdtempSync(path.join(tmpdir(), 'bm-models-'));
	try {
		const unpacked = spawnSync('tar', ['--use-compress-program=unzstd', '-xf', bundle, '-C', stage], {
			encoding: 'utf8',
			timeout: 1_800_000
		});
		if (unpacked.status !== 0) {
			return { ok: false, log: transcript(unpacked.stdout, unpacked.stderr) || 'tar failed' };
		}
		const manPath = path.join(stage, 'manifest.json');
		if (!existsSync(manPath)) {
			return { ok: false, log: 'bundle holds no manifest.json — nothing to verify against' };
		}
		const m = JSON.parse(readFileSync(manPath, 'utf8')) as ModelsManifest;
		const want = m.files;
		const got = new Set(walk(stage));
		got.delete('manifest.json');
		const listed = new Set(Object.keys(want));
		const extra = [...got].filter((f) => !listed.has(f)).sort();
		const missing = [...listed].filter((f) => !got.has(f)).sort();
		const lines: string[] = [];
		for (const name of extra) lines.push(`  UNLISTED ${name} — in the bundle, absent from the manifest`);
		for (const name of missing) lines.push(`  MISSING ${name} — listed by the manifest, absent from the bundle`);
		if (extra.length || missing.length) {
			return {
				ok: false,
				log: [
					...lines,
					`${listed.size} listed, ${got.size} present — bundle and manifest disagree`
				].join('\n')
			};
		}
		const bad: string[] = [];
		for (const name of Object.keys(want).sort()) {
			if (sha256File(path.join(stage, name)) !== want[name].sha256) bad.push(name);
		}
		if (bad.length) {
			return {
				ok: false,
				log: [...bad.map((n) => `  CORRUPT ${n}`), `${bad.length} file(s) do not match the manifest`].join(
					'\n'
				)
			};
		}
		const hash = manifestHash(m);
		return {
			ok: true,
			log: [
				`${Object.keys(want).length} files verified, none unlisted`,
				`hash ${hash}`,
				`tag  models-v${short(hash)}`
			].join('\n')
		};
	} finally {
		rmSync(stage, { recursive: true, force: true });
	}
}

/** Relative paths of every file under `dir`. */
function walk(dir: string, base = dir): string[] {
	const out: string[] = [];
	for (const item of readdirSync(dir, { withFileTypes: true })) {
		const full = path.join(dir, item.name);
		if (item.isDirectory()) out.push(...walk(full, base));
		else if (item.isFile()) out.push(path.relative(base, full));
	}
	return out;
}
