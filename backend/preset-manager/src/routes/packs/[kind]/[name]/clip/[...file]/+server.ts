//! Clip playback for the sounds editor: bytes from whichever tree is being
//! edited, in the shape an `<audio>` element asks for.

import path from 'node:path';
import { error } from '@sveltejs/kit';
import { serveAudio } from '#lib/audio-file';
import { AUDIO, clipFile, treeFor } from '#lib/sounds';
import type { RequestHandler } from './$types';

export const GET: RequestHandler = async ({ params, request }) => {
	const abs = clipFile(treeFor(params.kind, params.name), params.file);
	if (!abs) throw error(404, 'no such clip');
	const type = AUDIO[path.extname(abs).toLowerCase()] ?? 'application/octet-stream';
	return serveAudio(abs, type, request.headers.get('range'));
};
