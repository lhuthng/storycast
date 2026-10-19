//! Every pack in the bucket, its working copies, and the two writes that start
//! here: pull a published version down, or start a voices pack of its own.

import { fail, redirect } from '@sveltejs/kit';
import { download, inventory } from '#lib/packs';
import { createPack } from '#lib/voice-pack';
import type { Actions, PageServerLoad } from './$types';

export const load: PageServerLoad = async ({ url }) => {
	const kind = url.searchParams.get('kind') ?? '';
	// ?refresh=1 is the reason to ask the bucket again; otherwise the listing is cached.
	const inv = await inventory(url.searchParams.has('refresh'));
	return {
		error: inv.error,
		objects: inv.objects,
		kinds: [...new Set(inv.packs.map((p) => p.kind))],
		kind,
		packs: kind ? inv.packs.filter((p) => p.kind === kind) : inv.packs
	};
};

export const actions: Actions = {
	createVoice: async ({ request }) => {
		const form = await request.formData();
		const name = String(form.get('name') ?? '')
			.trim()
			.toLowerCase();
		const result = createPack(name, String(form.get('label') ?? ''));
		if (!result.ok) return fail(400, { ok: false, pack: `voices/${name}`, log: result.msg });
		throw redirect(303, `/packs/voices/${encodeURIComponent(name)}`);
	},

	download: async ({ request }) => {
		const form = await request.formData();
		const kind = String(form.get('kind') ?? '');
		const name = String(form.get('name') ?? '');
		const force = form.get('force') === 'on';
		const pack = `${kind}/${name}`;
		const result = await download(kind, name, force);
		if (!result.ok) return fail(400, { ok: false, pack, log: result.log });
		return { ok: true, pack, log: result.log };
	}
};
