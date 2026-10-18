//! What the shell needs on every page: whether the bucket is reachable at all.

import { status, type R2Status } from '#lib/r2';
import type { LayoutServerLoad } from './$types';

export const load: LayoutServerLoad = () => {
	let r2: R2Status;
	try {
		r2 = status();
	} catch (e) {
		r2 = {
			listed: false,
			where: 'nowhere',
			bucket: '-',
			base: '-',
			message: (e as Error).message
		};
	}
	return { r2 };
};
