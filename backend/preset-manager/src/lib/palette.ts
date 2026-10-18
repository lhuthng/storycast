//! The mood vocabulary: the keys of `scene-map.json`'s `music_palette`, which is
//! what every script names in `segments[].music`.
//!
//! A mood is the only name in a pack that a rename has to **travel**. A sound
//! key is held by nobody — a script names a mood, the palette turns it into
//! tags, the pool answers the tags — so renaming a sound is a local edit. A
//! mood is written into every script that used it, so renaming one is a release
//! act: the pack records it in its history and the pipeline rewrites the
//! workspaces on update.

import path from 'node:path';
import { assetsOf, checkKey, type Tree } from './sounds';
import { readJson, writeJson } from './json';
import { recordRename } from './migrations';

export interface Mood {
	mood: string;
	tags: string[];
	note: string;
	/** The name this mood had before a rename, when it has one. */
	id: string | null;
}

function paletteOf(doc: Record<string, unknown>): Record<string, unknown> {
	const palette = doc.music_palette;
	if (!palette || typeof palette !== 'object') {
		throw new Error('scene-map.json has no music_palette');
	}
	return palette as Record<string, unknown>;
}

/** Every mood the pack answers, in file order. `_note` is prose, not a mood. */
export async function moods(tree: Tree): Promise<Mood[]> {
	const doc = await readJson(path.join(assetsOf(tree), 'scene-map.json'));
	const palette = paletteOf(doc);
	const out: Mood[] = [];
	for (const [mood, raw] of Object.entries(palette)) {
		if (mood.startsWith('_')) continue;
		const entry = (raw ?? {}) as Record<string, unknown>;
		out.push({
			mood,
			tags: Array.isArray(entry.tags) ? entry.tags.filter((t) => typeof t === 'string') : [],
			note: typeof entry.note === 'string' ? entry.note : '',
			id: typeof entry.id === 'string' && entry.id !== '' ? entry.id : null
		});
	}
	return out;
}

/**
 * Rename a mood, keeping its identity and recording the rename.
 *
 * The `id` is what makes this a rename rather than a new mood: the track roll
 * seeds on it, so the audio of every chapter that uses this mood stays put, and
 * the design stamp does not move — the scripts are rewritten, nothing re-merges.
 *
 * `none` is refused. It is not a mood with tags but a word the pipeline checks
 * by name (`plan_music` skips it, the analyzer emits it), so renaming it would
 * leave those two branches pointing at nothing.
 */
export async function renameMood(
	tree: Tree,
	pack: string,
	from: string,
	to: string
): Promise<void> {
	checkKey(from, 'mood');
	checkKey(to, 'mood');
	if (from === to) return;
	if (from === 'none' || to === 'none') {
		throw new Error('`none` is the pipeline’s word for silence — it cannot be renamed');
	}
	const file = path.join(assetsOf(tree), 'scene-map.json');
	const doc = await readJson(file);
	const palette = paletteOf(doc);
	if (!(from in palette)) throw new Error(`${from} is not a mood in ${pack}`);
	if (to in palette) throw new Error(`${to} is already a mood in ${pack}`);

	const before = structuredClone(doc);
	const entry = { ...(palette[from] as Record<string, unknown>) };
	if (typeof entry.id !== 'string' || entry.id === '') entry.id = from;
	const next: Record<string, unknown> = {};
	for (const [key, value] of Object.entries(palette)) {
		next[key === from ? to : key] = key === from ? entry : value;
	}
	doc.music_palette = next;
	await writeJson(file, doc);

	// A palette the history does not mention is the failure this module exists
	// to prevent, so the edit is undone rather than left half-recorded.
	try {
		await recordRename(pack, 'palette', from, to);
	} catch (e) {
		await writeJson(file, before);
		throw e;
	}
}
