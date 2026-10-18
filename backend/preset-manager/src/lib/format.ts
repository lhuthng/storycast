//! Display helpers for the admin surfaces.

/** Bytes as someone reads them: 1.4 MB, 380 MB. */
export function bytes(n: number): string {
	if (!Number.isFinite(n) || n < 0) return '-';
	const units = ['B', 'KB', 'MB', 'GB'];
	let i = 0;
	let v = n;
	while (v >= 1024 && i < units.length - 1) {
		v /= 1024;
		i++;
	}
	return `${i === 0 ? v : v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

/** A date as `2026-10-11 09:40`, local time, or `-`. */
export function when(value: string | number | Date | undefined | null): string {
	if (value === undefined || value === null || value === '') return '-';
	const d = new Date(value);
	if (Number.isNaN(d.getTime())) return '-';
	const pad = (n: number) => String(n).padStart(2, '0');
	return (
		`${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
		` ${pad(d.getHours())}:${pad(d.getMinutes())}`
	);
}

/** Today as the publish script's `auto` version stamp. */
export function today(): string {
	return new Date().toISOString().slice(0, 10);
}
