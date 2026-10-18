//! R2: the bucket the packs are published to, its public base, and the
//! credentials the server may use to list it. Credentials come from the dev
//! server's env, or from the checkout's own `.env` — the same file
//! `tools/preset-publish.sh` documents, so there is one place to fill in.

import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { repoRoot } from './files';

/** R2 refuses a bare `Python-urllib`; every fetch says who it is. */
export const USER_AGENT = 'storycast-preset-fetch/1.0';

const DEFAULT_BUCKET = 'storycast-presets';
const DEFAULT_BASE = 'https://storycast-presets.huuthangle.site';

export interface R2Config {
	accountId: string;
	accessKeyId: string;
	secretAccessKey: string;
	bucket: string;
	base: string;
	endpoint: string;
}

export interface R2Status {
	listed: boolean;
	where: 'process env' | '.env' | 'nowhere';
	bucket: string;
	base: string;
	message: string;
}

let dotenv: Record<string, string> | null = null;

function readDotenv(): Record<string, string> {
	const out: Record<string, string> = {};
	let text: string;
	try {
		text = readFileSync(path.join(repoRoot(), '.env'), 'utf8');
	} catch {
		return out;
	}
	for (const line of text.split('\n')) {
		const m = /^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)$/.exec(line);
		if (!m) continue;
		out[m[1]] = m[2].trim().replace(/^(['"])(.*)\1$/, '$2');
	}
	return out;
}

/** `process.env` first, the checkout's `.env` second. */
export function setting(name: string): string | undefined {
	const fromEnv = process.env[name];
	if (fromEnv) return fromEnv;
	if (dotenv === null) dotenv = readDotenv();
	return dotenv[name];
}

/** The bucket and its keys, or null when the checkout has none. */
export function config(): R2Config | null {
	const accountId = setting('R2_ACCOUNT_ID');
	const accessKeyId = setting('R2_ACCESS_KEY_ID');
	const secretAccessKey = setting('R2_SECRET_ACCESS_KEY');
	if (!accountId || !accessKeyId || !secretAccessKey) return null;
	return {
		accountId,
		accessKeyId,
		secretAccessKey,
		bucket: setting('R2_BUCKET') || DEFAULT_BUCKET,
		base: (setting('R2_PUBLIC_BASE') || DEFAULT_BASE).replace(/\/+$/, ''),
		endpoint: `https://${accountId}.r2.cloudflarestorage.com`
	};
}

/** What the shell shows about the bucket, without touching the network. */
export function status(): R2Status {
	const cfg = config();
	const bucket = cfg?.bucket ?? setting('R2_BUCKET') ?? DEFAULT_BUCKET;
	const base = (setting('R2_PUBLIC_BASE') || DEFAULT_BASE).replace(/\/+$/, '');
	if (!cfg) {
		return {
			listed: false,
			where: 'nowhere',
			bucket,
			base,
			message: 'No R2 credentials, set R2_ACCOUNT_ID, R2_ACCESS_KEY_ID and R2_SECRET_ACCESS_KEY'
		};
	}
	const where = process.env.R2_ACCOUNT_ID ? 'process env' : '.env';
	return { listed: true, where, bucket, base, message: `Listing ${cfg.bucket}` };
}

export interface BucketObject {
	key: string;
	bytes: number;
	modified: string;
}

/** Every object under a prefix, oldest first. Throws with the CLI's own words. */
export function listObjects(prefix = 'preset/'): BucketObject[] {
	const cfg = config();
	if (!cfg) throw new Error('R2 credentials are not set');
	const r = spawnSync(
		'aws',
		[
			'--endpoint-url',
			cfg.endpoint,
			's3api',
			'list-objects-v2',
			'--bucket',
			cfg.bucket,
			'--prefix',
			prefix,
			'--output',
			'json'
		],
		{
			encoding: 'utf8',
			timeout: 60_000,
			// R2 speaks its own regions; an ambient AWS_REGION is rejected otherwise.
			env: {
				...process.env,
				AWS_ACCESS_KEY_ID: cfg.accessKeyId,
				AWS_SECRET_ACCESS_KEY: cfg.secretAccessKey,
				AWS_REGION: 'auto',
				AWS_DEFAULT_REGION: 'auto'
			}
		}
	);
	if (r.error) throw new Error(`aws CLI cannot run: ${r.error.message}`);
	if (r.status !== 0) {
		const detail = (r.stderr || '').trim().split('\n').filter(Boolean).slice(-2).join(' ');
		throw new Error(detail || 'aws s3api list-objects-v2 failed');
	}
	const doc = JSON.parse(r.stdout || '{}') as {
		Contents?: { Key: string; Size: number; LastModified: string }[];
	};
	return (doc.Contents ?? []).map((c) => ({
		key: c.Key,
		bytes: c.Size ?? 0,
		modified: c.LastModified ?? ''
	}));
}

/** A public JSON document, or null when the key is not there. */
export async function getJson<T>(url: string): Promise<T | null> {
	const r = await fetch(url, { headers: { 'user-agent': USER_AGENT } });
	if (r.status === 404) return null;
	if (!r.ok) throw new Error(`${url}: ${r.status} ${r.statusText}`);
	return (await r.json()) as T;
}

/** Fetch one public key to a path, streaming, and fail loudly on a bad status. */
export function fetchToFile(url: string, dest: string): void {
	if (!existsSync(path.dirname(dest))) throw new Error(`no directory for ${dest}`);
	const r = spawnSync(
		'curl',
		['-fSL', '--retry', '2', '--retry-delay', '2', '-A', USER_AGENT, '-o', dest, url],
		{ encoding: 'utf8', timeout: 900_000 }
	);
	if (r.error) throw new Error(`curl cannot run: ${r.error.message}`);
	if (r.status !== 0) {
		const detail = (r.stderr || '').trim().split('\n').filter(Boolean).slice(-1)[0];
		throw new Error(detail || `curl failed for ${url}`);
	}
}

/** `sha256sum` where it exists, `shasum -a 256` everywhere else. */
export function hashFile(file: string): string {
	for (const argv of [
		['-a', '256', file],
		[file]
	]) {
		const bin = argv[0] === '-a' ? 'shasum' : 'sha256sum';
		const r = spawnSync(bin, argv, { encoding: 'utf8' });
		if (r.error || r.status !== 0) continue;
		const hex = (r.stdout || '').trim().split(/\s+/)[0];
		if (/^[0-9a-f]{64}$/.test(hex)) return hex;
	}
	throw new Error(`cannot hash ${file} (need shasum or sha256sum)`);
}
