//! Releasing: build a pack bundle from the live tree with `bm-inductor`, then
//! hand it to `tools/preset-publish.sh` — the only thing here that writes to
//! R2, and the only thing that needs the account's write credentials.

import { spawnSync } from 'node:child_process';
import { existsSync, readdirSync, statSync } from 'node:fs';
import path from 'node:path';
import { repoRoot } from './files';
import { forgetInventory } from './packs';
import { setting } from './r2';

const WORD = /^[A-Za-z0-9._-]+$/;
/** Enough of a transcript to see what happened: the result is always last. */
const TAIL_LINES = 30;

export interface Bundle {
	rel: string;
	bytes: number;
	modified: string;
}

export interface Outcome {
	ok: boolean;
	log: string;
}

function tail(text: string): string {
	const lines = text.split('\n').filter((l) => l.trim() !== '');
	if (lines.length <= TAIL_LINES) return lines.join('\n');
	return ['…', ...lines.slice(-TAIL_LINES)].join('\n');
}

function transcript(stdout: string | null, stderr: string | null): string {
	return tail(
		[(stdout ?? '').trim(), (stderr ?? '').trim()].filter(Boolean).join('\n')
	);
}

/** Every archive the live tree has built, ready to publish. */
export function bundles(): Bundle[] {
	const root = repoRoot();
	const out: Bundle[] = [];
	for (const dir of ['profiles/pack', 'profiles/adapter', 'engines']) {
		const abs = path.join(root, dir);
		if (!existsSync(abs)) continue;
		for (const rel of walk(abs).filter((f) => /\.(tar\.zst|tar\.gz|tar\.zstd)$/.test(f))) {
			const full = path.join(root, rel);
			const s = statSync(full);
			out.push({ rel, bytes: s.size, modified: s.mtime.toISOString() });
		}
	}
	return out.sort((a, b) => (a.rel < b.rel ? -1 : 1));
}

/** Relative paths of every file under a directory, to a sane depth. */
function walk(dir: string, depth = 0): string[] {
	if (depth > 4) return [];
	const out: string[] = [];
	let items;
	try {
		items = readdirSync(dir, { withFileTypes: true });
	} catch {
		return out;
	}
	for (const item of items) {
		const full = path.join(dir, item.name);
		if (item.isDirectory()) out.push(...walk(full, depth + 1));
		else if (item.isFile()) out.push(path.relative(repoRoot(), full));
	}
	return out;
}

/**
 * `tools/profile.sh pack <name> [--dep] [--force]`, which writes
 * `profiles/pack/<name>.tar.zst`. The manifest inside it comes from
 * `bm-inductor profile manifest` — the shell script owns tar/zstd, the binary
 * owns the manifest, and the split is deliberate (see the script's own head).
 *
 * `dep` releases the pack authored at `assets/_extends/<name>` as a flat,
 * sanitized root pack; without it, the live composition is packed. The script
 * refuses `--dep` for a composed pack and refuses a stale tree without `force`.
 */
export function buildBundle(name: string, dep: boolean, force: boolean): Outcome {
	if (!WORD.test(name)) return { ok: false, log: `bad pack name: ${name}` };
	const root = repoRoot();
	if (!existsSync(path.join(root, 'tools', 'profile.sh'))) {
		return { ok: false, log: 'no tools/profile.sh in this checkout' };
	}
	const argv = ['tools/profile.sh', 'pack', name];
	if (dep) argv.push('--dep');
	if (force) argv.push('--force');
	const r = spawnSync('bash', argv, { cwd: root, encoding: 'utf8', timeout: 900_000 });
	const log = transcript(r.stdout, r.stderr);
	if (r.status !== 0) return { ok: false, log: log || `profile.sh exited ${r.status}` };
	const rel = path.join('profiles', 'pack', `${name}.tar.zst`);
	const abs = path.join(root, rel);
	return {
		ok: true,
		log: [log, existsSync(abs) ? `built ${rel} (${statSync(abs).size} bytes)` : 'no archive appeared']
			.filter(Boolean)
			.join('\n')
	};
}

export interface PublishInput {
	kind: string;
	name: string;
	version: string;
	bundle: string;
}

/** Upload one version and repoint `latest.json`. Never accepts a browser's credentials. */
export function publish(input: PublishInput): Outcome {
	const { kind, name, bundle } = input;
	const version = input.version.trim() || 'auto';
	if (!WORD.test(kind)) return { ok: false, log: `bad kind: ${kind}` };
	if (!WORD.test(name)) return { ok: false, log: `bad name: ${name}` };
	if (!WORD.test(version)) return { ok: false, log: `bad version: ${version}` };
	if (path.isAbsolute(bundle) || bundle.split('/').includes('..')) {
		return { ok: false, log: `refused path: ${bundle}` };
	}
	const root = repoRoot();
	const abs = path.join(root, bundle);
	if (!existsSync(abs) || !statSync(abs).isFile()) return { ok: false, log: `no such file: ${bundle}` };

	const accountId = setting('R2_ACCOUNT_ID');
	const accessKeyId = setting('R2_ACCESS_KEY_ID');
	const secretAccessKey = setting('R2_SECRET_ACCESS_KEY');
	if (!accountId || !accessKeyId || !secretAccessKey) {
		return { ok: false, log: 'no R2 credentials: set R2_ACCOUNT_ID, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY' };
	}

	const env: NodeJS.ProcessEnv = {
		...process.env,
		R2_ACCOUNT_ID: accountId,
		R2_ACCESS_KEY_ID: accessKeyId,
		R2_SECRET_ACCESS_KEY: secretAccessKey
	};
	for (const name of ['R2_BUCKET', 'R2_PUBLIC_BASE']) {
		const v = setting(name);
		if (v) env[name] = v;
	}

	const r = spawnSync('bash', ['tools/preset-publish.sh', kind, name, version, bundle], {
		cwd: root,
		env,
		encoding: 'utf8',
		// A model bundle is 380 MB out and the script verifies what it uploaded.
		timeout: 900_000
	});
	const log = transcript(r.stdout, r.stderr);
	if (r.status !== 0) return { ok: false, log: log || `preset-publish.sh exited ${r.status}` };
	// The bucket just moved; the next page load must not serve the old listing.
	forgetInventory();
	return { ok: true, log };
}
