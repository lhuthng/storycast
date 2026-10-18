//! Every pack in the bucket, and the download that pulls one down here.

import { fail } from '@sveltejs/kit';
import { download, inventory } from '#lib/packs';
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
