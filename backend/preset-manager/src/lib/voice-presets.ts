//! The shape of a voice preset and what a save must refuse: a label, advisory
//! categories, and tagged clone voices. The files themselves live with the
//! pack — `voice-pack.ts` reads and writes them.

export const VERSION = 1;

export interface PresetVoice {
	voice: string;
	tags: string[];
}

export interface VoicePreset {
	version: number;
	label: string;
	category: string[];
	voices: PresetVoice[];
}

/** Canonical tag spelling, matching `bm-core`'s `voices::tags`. */
export function normalizeTag(raw: string): string {
	const tag = raw.trim().toLowerCase().split(/\s+/).filter(Boolean).join('-');
	if (tag === 'elderly') return 'old';
	if (tag === 'powerful') return 'strong';
	return tag;
}

export function normalizeTags(raw: string[]): string[] {
	const out: string[] = [];
	for (const item of raw) {
		const tag = normalizeTag(item);
		if (tag && !out.includes(tag)) out.push(tag);
	}
	return out;
}

// Gender is not a conflict: one voice may be offered for male and female roles.
const CONFLICTS: [string, string][] = [
	['child', 'adult'],
	['child', 'old'],
	['young', 'old'],
	['strong', 'weak']
];

export function tagConflict(tags: string[]): [string, string] | null {
	for (const [a, b] of CONFLICTS) {
		if (tags.includes(a) && tags.includes(b)) return [a, b];
	}
	return null;
}

/**
 * A save is refused rather than normalized into something the operator did
 * not mean: duplicate voices and contradictory tags are mistakes, not taste.
 */
export function validatePreset(preset: VoicePreset): VoicePreset {
	if (preset.version !== VERSION) {
		throw new Error(`voice preset version ${preset.version} is not supported (this build writes ${VERSION})`);
	}
	const label = String(preset.label ?? '').trim();
	if (!label) throw new Error('a voice preset needs a label');
	const category = normalizeTags(Array.isArray(preset.category) ? preset.category : []);
	const voices: PresetVoice[] = [];
	const seen = new Set<string>();
	for (const raw of Array.isArray(preset.voices) ? preset.voices : []) {
		const voice = String(raw.voice ?? '').trim();
		if (!voice) throw new Error('every voice needs a name');
		if (seen.has(voice)) throw new Error(`${voice} is listed twice`);
		seen.add(voice);
		const tags = normalizeTags(Array.isArray(raw.tags) ? raw.tags : []);
		const conflict = tagConflict(tags);
		if (conflict) {
			throw new Error(`${voice} carries both ${conflict[0]} and ${conflict[1]}`);
		}
		voices.push({ voice, tags });
	}
	return { version: VERSION, label, category, voices };
}

export function parsePreset(text: string): VoicePreset {
	return validatePreset(JSON.parse(text) as VoicePreset);
}
