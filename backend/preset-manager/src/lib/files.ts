//! The preset files preset-manager may read and write, and the repo root
//! they live under. Everything else is refused: an editor that can write any
//! path is a footgun, not a tool.

import { existsSync } from 'node:fs';
import { stat } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export interface ManagedFile {
	id: string;
	label: string;
	rel: string;
	/** What reads this file, so the editor can say why it matters. */
	what: string;
}

export const MANAGED: ManagedFile[] = [
	{
		id: 'presets',
		label: 'Profile presets',
		rel: 'profiles/presets.json',
		what: 'The names a workspace is stamped from: pack × adapter × engine.'
	},
	{
		id: 'music-pool',
		label: 'Music pool',
		rel: 'assets/music-pool.json',
		what: 'Every track a mood can reach, and the palette that names moods.'
	},
	{
		id: 'effect-pool',
		label: 'Effect pool',
		rel: 'assets/effect-pool.json',
		what: 'The place beds a scene rule can pick.'
	},
	{
		id: 'inject-pool',
		label: 'Inject pool',
		rel: 'assets/inject-pool.json',
		what: 'Spot sounds: the tag is the API the model writes against.'
	},
	{
		id: 'scene-map',
		label: 'Scene map',
		rel: 'assets/scene-map.json',
		what: 'Scene labels to effect, reverb, music and ducking. Rules, first match wins.'
	},
	{
		id: 'tag-aliases',
		label: 'Tag aliases',
		rel: 'assets/tag-aliases.json',
		what: "The model's words for the pool's nouns."
	},
	{
		id: 'pack',
		label: 'Pack deps',
		rel: 'assets/pack.json',
		what: 'This composition\'s dependencies, weakest first.'
	},
	{
		id: 'extends',
		label: 'Pack extends',
		rel: 'assets/_extends.json',
		what: 'The resolved composition record: which dep filled which key.'
	},
	{
		id: 'voices',
		label: 'Voice catalogue',
		rel: 'voices.default.json',
		what: 'The voices a workspace can cast from.'
	}
];

/** The checkout root: `PRESET_ROOT` wins, otherwise the dir above `backend/`. */
export function repoRoot(): string {
	const here = path.dirname(fileURLToPath(import.meta.url));
	const cands = [
		process.env.PRESET_ROOT,
		path.resolve(process.cwd(), '..', '..'),
		path.resolve(here, '..', '..', '..', '..')
	];
	for (const c of cands) {
		if (c && existsSync(path.join(c, 'profiles', 'presets.json'))) return c;
	}
	throw new Error('cannot locate the repo root (expected profiles/presets.json above preset-manager)');
}

/** The absolute path of a managed file, or a throw for anything else. */
export function managedPath(id: string): string {
	const m = MANAGED.find((f) => f.id === id);
	if (!m) throw new Error(`unknown file: ${id}`);
	return path.join(repoRoot(), m.rel);
}

export interface ManagedFileInfo extends ManagedFile {
	exists: boolean;
	bytes: number;
	mtime: string | null;
}

/** Every managed file with its size on disk, in the declared order. */
export async function managedFiles(): Promise<ManagedFileInfo[]> {
	const root = repoRoot();
	return Promise.all(
		MANAGED.map(async (m) => {
			try {
				const s = await stat(path.join(root, m.rel));
				return { ...m, exists: true, bytes: s.size, mtime: s.mtime.toISOString() };
			} catch {
				return { ...m, exists: false, bytes: 0, mtime: null };
			}
		})
	);
}
