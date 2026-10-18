//! Clip playback for the sounds editor: bytes from whichever tree is being
//! edited, in the shape an `<audio>` element asks for.

import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { error } from '@sveltejs/kit';
import { AUDIO, clipFile, treeFor } from '#lib/sounds';
import type { RequestHandler } from './$types';

const RANGE = /^bytes=(\d*)-(\d*)$/;

export const GET: RequestHandler = async ({ params, request }) => {
	const abs = clipFile(treeFor(params.kind, params.name), params.file);
	if (!abs) throw error(404, 'no such clip');

	const bytes = await readFile(abs);
	const size = bytes.length;
	const type = AUDIO[path.extname(abs).toLowerCase()] ?? 'application/octet-stream';
	// Local edits rewrite these files, so the bytes are never worth caching.
	const base = { 'content-type': type, 'accept-ranges': 'bytes', 'cache-control': 'no-store' };

	// Safari asks for a one-byte range before it will play anything.
	const match = RANGE.exec(request.headers.get('range') ?? '');
	if (match && (match[1] !== '' || match[2] !== '')) {
		const start = match[1] === '' ? Math.max(0, size - Number(match[2])) : Number(match[1]);
		const end = match[1] === '' || match[2] === '' ? size - 1 : Math.min(Number(match[2]), size - 1);
		if (start > end || start >= size) {
			return new Response(null, { status: 416, headers: { 'content-range': `bytes */${size}` } });
		}
		const slice = new Uint8Array(bytes.subarray(start, end + 1));
		return new Response(slice, {
			status: 206,
			headers: { ...base, 'content-range': `bytes ${start}-${end}/${size}`, 'content-length': `${slice.length}` }
		});
	}

	return new Response(new Uint8Array(bytes), {
		headers: { ...base, 'content-length': `${size}` }
	});
};
