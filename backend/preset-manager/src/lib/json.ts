//! Reading and writing the JSON registries without moving anything: an editor
//! that reorders keys produces a diff nobody meant to make.

import { readFile } from 'node:fs/promises';
import { writeFile } from 'node:fs/promises';

export async function readJson(file: string): Promise<Record<string, unknown>> {
	return JSON.parse(await readFile(file, 'utf8')) as Record<string, unknown>;
}

/** Normalized on the way out: two-space indent, one trailing newline. */
export async function writeJson(file: string, doc: unknown): Promise<void> {
	await writeFile(file, `${JSON.stringify(doc, null, 2)}\n`);
}

/**
 * The editor's values in the order the entry already had them, so a save that
 * changes nothing writes the same bytes. Keys this editor does not know about
 * come along.
 */
export function orderLike(
	prev: Record<string, unknown>,
	values: Record<string, unknown>
): Record<string, unknown> {
	const out: Record<string, unknown> = {};
	for (const key of Object.keys(prev)) if (key in values) out[key] = values[key];
	for (const key of Object.keys(values)) if (!(key in out)) out[key] = values[key];
	return out;
}

export function str(value: unknown): string {
	return typeof value === 'string' ? value : '';
}

/** A number field, or undefined when blank — an absent key, not a zero. */
export function num(value: unknown): number | undefined {
	const raw = str(value).trim();
	if (raw === '') return undefined;
	const n = Number(raw);
	return Number.isFinite(n) ? n : undefined;
}
