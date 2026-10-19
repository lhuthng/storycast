//! The voices pack: one preset per pack, published under `preset/voices/<name>/`.
//! A pack is `voices/<name>/preset.json` beside the clones its slots name, so
//! the pack name and the preset are the same thing. Edits land in the working
//! copy under `.bm/packs/voices/<name>/<version>/`; publishing packs that tree.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
	cpSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	readdirSync,
	rmSync,
	writeFileSync
} from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { repoRoot } from './files';
import { PRESET_FILE, presetPath, workDir, workRoot } from './packs';
import { config } from './r2';
import { VERSION, parsePreset, validatePreset, type VoicePreset } from './voice-presets';

export interface PackedPreset extends VoicePreset {
	/** The pack's name; the file it was read from is `voices/<name>/preset.json`. */
	file: string;
}

const SLUG = /^[a-z0-9][a-z0-9-]*$/;

/** The newest downloaded copy of the pack, or null when none holds a preset. */
export function packTree(name: string, version?: string): string | null {
	const base = path.join(workRoot(), 'voices', name);
	let versions: string[];
	try {
		versions = readdirSync(base, { withFileTypes: true })
			.filter((d) => d.isDirectory())
			.map((d) => d.name)
			.sort()
			.reverse();
	} catch {
		return null;
	}
	const pick = version ? [version] : versions;
	for (const v of pick) {
		const dir = workDir('voices', name, v);
		if (existsSync(presetPath(dir, name))) return dir;
	}
	return null;
}

/** Every pack a checkout holds a working copy of, in name order. */
export function localPacks(): string[] {
	let names: string[];
	try {
		names = readdirSync(path.join(workRoot(), 'voices'), { withFileTypes: true })
			.filter((d) => d.isDirectory())
			.map((d) => d.name);
	} catch {
		return [];
	}
	return names.filter((n) => packTree(n) !== null).sort();
}

/** The pack's preset, or the reason it could not be read. */
export function readPackedPreset(
	tree: string,
	name: string
): { preset: PackedPreset | null; error: string | null } {
	const file = presetPath(tree, name);
	try {
		return { preset: { ...parsePreset(readFileSync(file, 'utf8')), file: name }, error: null };
	} catch (e) {
		return { preset: null, error: `${file}: ${(e as Error).message}` };
	}
}

/**
 * A slot may name a clone, but it does not have to. An empty name — or one
 * still reading `unnamed-*` — is a placeholder, renumbered `unnamed-N` in file
 * order so a dropped row leaves no gap and no two slots share a name.
 */
export function nameTheUnnamed(voices: {
	voice?: unknown;
	tags?: unknown;
}[]): { voice: string; tags: string[] }[] {
	let n = 0;
	return voices.map((v) => {
		const voice = String(v.voice ?? '').trim();
		const tags = Array.isArray(v.tags) ? v.tags.map(String) : [];
		return { voice: voice && !voice.startsWith('unnamed-') ? voice : `unnamed-${++n}`, tags };
	});
}

export interface SaveResult {
	ok: boolean;
	msg: string;
}

/** Write the pack's preset, normalized. */
export function savePackedPreset(
	tree: string,
	name: string,
	input: { label: string; category?: string[]; voices: unknown }
): SaveResult {
	let voices: unknown = input.voices;
	if (typeof voices === 'string') {
		try {
			voices = JSON.parse(voices);
		} catch {
			return { ok: false, msg: 'voices are not valid JSON' };
		}
	}
	if (!Array.isArray(voices)) return { ok: false, msg: 'voices must be a list' };
	let preset: VoicePreset;
	try {
		preset = validatePreset({
			version: VERSION,
			label: input.label,
			category: input.category ?? [],
			voices: nameTheUnnamed(voices as { voice?: unknown; tags?: unknown }[])
		});
	} catch (e) {
		return { ok: false, msg: (e as Error).message };
	}
	const file = presetPath(tree, name);
	try {
		mkdirSync(path.dirname(file), { recursive: true });
		writeFileSync(file, `${JSON.stringify(preset, null, 2)}\n`);
	} catch (e) {
		return { ok: false, msg: (e as Error).message };
	}
	return { ok: true, msg: `saved voices/${name}/${PRESET_FILE} (${preset.voices.length} voice(s))` };
}

/** Start a pack: a working copy with an empty preset. An existing copy is refused. */
export function createPack(name: string, label: string): SaveResult {
	if (!SLUG.test(name)) {
		return { ok: false, msg: 'pack name must be lowercase letters, digits and hyphens' };
	}
	if (packTree(name)) return { ok: false, msg: `voices/${name} is already here` };
	const tree = workDir('voices', name, new Date().toISOString().slice(0, 10));
	const preset = validatePreset({
		version: VERSION,
		label: label.trim() || name,
		category: [],
		voices: []
	});
	const file = presetPath(tree, name);
	try {
		mkdirSync(path.dirname(file), { recursive: true });
		writeFileSync(file, `${JSON.stringify(preset, null, 2)}\n`);
	} catch (e) {
		return { ok: false, msg: (e as Error).message };
	}
	return { ok: true, msg: `created ${path.relative(repoRoot(), file)}` };
}

/** Remove the pack's preset from the working copy. */
export function dropPack(tree: string, name: string): void {
	rmSync(path.join(tree, 'voices', name), { recursive: true, force: true });
}

function sha256(file: string): string {
	return createHash('sha256').update(readFileSync(file)).digest('hex');
}

/** Clone name -> its directory, from `<dir>/voices/clones/<slug>/voice.json`. */
function dirsUnder(root: string): Map<string, string> {
	const out = new Map<string, string>();
	let dirs: string[];
	try {
		dirs = readdirSync(root, { withFileTypes: true })
			.filter((d) => d.isDirectory())
			.map((d) => d.name);
	} catch {
		return out;
	}
	for (const slug of dirs) {
		try {
			const doc = JSON.parse(readFileSync(path.join(root, slug, 'voice.json'), 'utf8')) as {
				name?: string;
			};
			const name = String(doc.name ?? '').trim();
			if (name) out.set(name, path.join(root, slug));
		} catch {
			/* a half-built clone carries no audio */
		}
	}
	return out;
}

/** Clone name -> its directory, from the pack's own `voices/clones/<slug>/`. */
export function cloneDirs(tree: string | null): Map<string, string> {
	return tree ? dirsUnder(path.join(tree, 'voices', 'clones')) : new Map();
}

/** The take file a pad or a publish reads for `name`, pack copy first. */
export function takeFile(tree: string | null, name: string): string | null {
	const dir = cloneDirs(tree).get(name);
	if (!dir) return null;
	const take = path.join(dir, 'vieneu-clean.wav');
	return existsSync(take) ? take : null;
}

/** Every clone name with a take on disk: the editor's pads and its datalist. */
export function takeNames(tree: string | null): string[] {
	return [...cloneDirs(tree).keys()]
		.filter((name) => takeFile(tree, name))
		.sort((a, b) => a.localeCompare(b));
}

function walk(dir: string, base = dir): string[] {
	const out: string[] = [];
	for (const item of readdirSync(dir, { withFileTypes: true })) {
		const full = path.join(dir, item.name);
		if (item.isDirectory()) out.push(...walk(full, base));
		else if (item.isFile()) out.push(path.relative(base, full));
	}
	return out;
}

/**
 * Stage the pack: its preset, and the take of every clone the slots name. The
 * stage is removed by the caller. Refuses a name with no clone behind it.
 */
export function stagePack(
	name: string,
	version = 'auto'
):
	| { ok: true; stage: string; archive: string; packed: string[]; files: number }
	| { ok: false; log: string } {
	const tree = packTree(name);
	if (!tree) return { ok: false, log: `no working copy of voices/${name}` };
	let preset: VoicePreset;
	try {
		preset = parsePreset(readFileSync(presetPath(tree, name), 'utf8'));
	} catch (e) {
		return { ok: false, log: (e as Error).message };
	}

	const stage = mkdtempSync(path.join(tmpdir(), 'voices-pack-'));
	const clones = cloneDirs(tree);
	const packed: string[] = [];
	const missing: string[] = [];
	for (const voice of preset.voices) {
		if (voice.voice.startsWith('unnamed-') || packed.includes(voice.voice)) continue;
		const src = clones.get(voice.voice);
		if (!src) {
			missing.push(voice.voice);
			continue;
		}
		const dest = path.join(stage, 'voices', 'clones', path.basename(src));
		mkdirSync(dest, { recursive: true });
		// The take is the sample; the reference stays a local re-bake input.
		for (const f of ['voice.json', 'vieneu-clean.wav']) {
			const from = path.join(src, f);
			if (existsSync(from)) cpSync(from, path.join(dest, f));
		}
		packed.push(voice.voice);
	}

	// Refuse before packing: a name with no clone behind it is an operator
	// mistake, not an archive worth writing.
	if (missing.length) {
		rmSync(stage, { recursive: true, force: true });
		return { ok: false, log: `not a built clone: ${missing.join(', ')}` };
	}

	const own = path.join(stage, 'voices', name);
	mkdirSync(own, { recursive: true });
	cpSync(presetPath(tree, name), path.join(own, PRESET_FILE));

	const manifest = {
		name,
		version: version === 'auto' ? new Date().toISOString().slice(0, 10) : version,
		deps: [],
		files: Object.fromEntries(walk(stage).map((f) => [f, sha256(path.join(stage, f))]))
	};
	writeFileSync(path.join(stage, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
	// `voices/clones` only exists once something was packed into it; naming a
	// missing member fails the whole tar.
	const members = ['manifest.json', `voices/${name}`];
	if (packed.length) members.push('voices/clones');
	const archive = path.join(stage, `${name}.tar.zst`);
	const tar = spawnSync('tar', ['--use-compress-program=zstd', '-cf', archive, ...members], {
		cwd: stage,
		encoding: 'utf8',
		timeout: 300_000
	});
	if (tar.status !== 0) {
		rmSync(stage, { recursive: true, force: true });
		return { ok: false, log: `tar failed: ${(tar.stderr || '').trim()}` };
	}
	return { ok: true, stage, archive, packed, files: walk(stage).length };
}

/** Pack the staged tree and publish it as `voices/<name>@<version>` to R2. */
export function publishVoicePack(name: string, version = 'auto'): { ok: boolean; log: string } {
	const cfg = config();
	if (!cfg) return { ok: false, log: 'no R2 credentials in .env' };
	const staged = stagePack(name, version);
	if (!staged.ok) return { ok: false, log: staged.log };
	const { stage, archive, packed, files } = staged;
	try {
		const publish = spawnSync(
			path.join(repoRoot(), 'tools', 'preset-publish.sh'),
			['voices', name, version, archive],
			{
				cwd: repoRoot(),
				encoding: 'utf8',
				timeout: 300_000,
				env: {
					...process.env,
					R2_ACCOUNT_ID: cfg.accountId,
					R2_ACCESS_KEY_ID: cfg.accessKeyId,
					R2_SECRET_ACCESS_KEY: cfg.secretAccessKey,
					R2_BUCKET: cfg.bucket,
					R2_PUBLIC_BASE: cfg.base
				}
			}
		);
		const head = `packed ${packed.length} voice(s) with audio (${files} files, ${(
			readFileSync(archive).length / 1e6
		).toFixed(1)} MB)`;
		const log = [head, (publish.stdout || '').trim(), (publish.stderr || '').trim()]
			.filter(Boolean)
			.join('\n');
		if (publish.error) return { ok: false, log: publish.error.message };
		return { ok: publish.status === 0, log };
	} finally {
		rmSync(stage, { recursive: true, force: true });
	}
}
