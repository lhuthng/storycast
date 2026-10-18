//! The rename history a pack ships beside its `assets/`.
//!
//! A rename is the one change a content hash cannot express: a workspace that
//! already wrote `music: romantic` into 143 segments has no way to learn the
//! mood is now called something else. So the pack says it outright, in
//! `profiles/pack/<name>.migrations.json`, and `tools/profile.sh pack` puts it
//! at the bundle root where `bm-inductor profile update` reads it.
//!
//! This module only *records* intent. Rewriting a workspace's scripts is the
//! pipeline's job (`bm_core::pack_migration::apply`), because only it knows
//! which tree is live.

import { existsSync, mkdirSync } from 'node:fs';
import path from 'node:path';
import { repoRoot } from './files';
import { readJson, writeJson } from './json';

/** One release's renames. Cumulative: a workspace that skipped one still lands. */
export interface Step {
	/** The release it shipped in, as a date. Informational: the pipeline ignores it. */
	version: string;
	palette: Record<string, string>;
	inject: Record<string, string>;
}

export interface Migrations {
	format: number;
	pack: string;
	history: Step[];
}

/** The newest document `bm_core::pack_migration` understands. */
export const FORMAT = 1;

export function historyPath(name: string): string {
	return path.join(repoRoot(), 'profiles', 'pack', `${name}.migrations.json`);
}

export async function readHistory(name: string): Promise<Migrations> {
	const file = historyPath(name);
	if (!existsSync(file)) return { format: FORMAT, pack: name, history: [] };
	const doc = (await readJson(file)) as unknown as Partial<Migrations>;
	return {
		format: doc.format ?? FORMAT,
		pack: doc.pack ?? name,
		history: Array.isArray(doc.history) ? doc.history : []
	};
}

/**
 * Append one rename. One step per rename rather than one per release: the
 * history is a log of what happened, and a chain (`a→b`, then `b→c`) is only
 * readable if both hops are in it — the pipeline composes them on read.
 */
export async function recordRename(
	name: string,
	layer: 'palette' | 'inject',
	from: string,
	to: string
): Promise<void> {
	const doc = await readHistory(name);
	const step: Step = { version: new Date().toISOString().slice(0, 10), palette: {}, inject: {} };
	step[layer][from] = to;
	doc.format = FORMAT;
	doc.pack = name;
	doc.history.push(step);
	mkdirSync(path.dirname(historyPath(name)), { recursive: true });
	await writeJson(historyPath(name), doc);
}

/** Every rename this pack has ever declared, oldest first, as `old -> new`. */
export function declared(doc: Migrations): string[] {
	return doc.history.flatMap((step) =>
		Object.entries({ ...step.palette, ...step.inject }).map(([from, to]) => `${from} -> ${to}`)
	);
}
