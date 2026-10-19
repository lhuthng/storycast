//! One audio file as bytes, in the shape an `<audio>` element asks for:
//! Range request honoured, nothing cached.

import { readFile } from 'node:fs/promises';

const RANGE = /^bytes=(\d*)-(\d*)$/;

/** The bytes of `file`, typed `type`, answering a single Range header. */
export async function serveAudio(
	file: string,
	type: string,
	range: string | null
): Promise<Response> {
	const bytes = await readFile(file);
	const size = bytes.length;
	// Local edits rewrite these files, so the bytes are never worth caching.
	const base = { 'content-type': type, 'accept-ranges': 'bytes', 'cache-control': 'no-store' };

	// Safari asks for a one-byte range before it will play anything.
	const match = RANGE.exec(range ?? '');
	if (match && (match[1] !== '' || match[2] !== '')) {
		const start = match[1] === '' ? Math.max(0, size - Number(match[2])) : Number(match[1]);
		const end =
			match[1] === '' || match[2] === '' ? size - 1 : Math.min(Number(match[2]), size - 1);
		if (start > end || start >= size) {
			return new Response(null, { status: 416, headers: { 'content-range': `bytes */${size}` } });
		}
		const slice = new Uint8Array(bytes.subarray(start, end + 1));
		return new Response(slice, {
			status: 206,
			headers: {
				...base,
				'content-range': `bytes ${start}-${end}/${size}`,
				'content-length': `${slice.length}`
			}
		});
	}

	return new Response(new Uint8Array(bytes), {
		headers: { ...base, 'content-length': `${size}` }
	});
}
