//! Overview: what the bucket holds, what this checkout authors, and what is
//! already downloaded or built here.

import { managedFiles } from '#lib/files';
import { authorPacks, inventory, workingCopies } from '#lib/packs';
import { bundles } from '#lib/release';
import type { PageServerLoad } from './$types';

export const load: PageServerLoad = async () => {
	const [inv, files] = await Promise.all([inventory(), managedFiles()]);
	return {
		inv,
		files: files.filter((f) => f.exists),
		authors: authorPacks(),
		copies: workingCopies(),
		bundles: bundles()
	};
};
