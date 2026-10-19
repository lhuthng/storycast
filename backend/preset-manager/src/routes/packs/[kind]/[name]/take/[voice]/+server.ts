//! One voice's take for the presets editor's play pads: the pack's own copy
//! of the clone.

import { error } from '@sveltejs/kit';
import { serveAudio } from '#lib/audio-file';
import { AUDIO } from '#lib/sounds';
import { packTree, takeFile } from '#lib/voice-pack';
import type { RequestHandler } from './$types';

export const GET: RequestHandler = async ({ params, request }) => {
	if (params.kind !== 'voices') throw error(404, 'only a voices pack has takes');
	const file = params.voice ? takeFile(packTree(params.name), params.voice) : null;
	if (!file) throw error(404, 'no take for that voice');
	return serveAudio(file, AUDIO['.wav'] ?? 'audio/wav', request.headers.get('range'));
};
