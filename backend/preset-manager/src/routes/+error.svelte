<script lang="ts">
	import { page } from '$app/state';
</script>

<svelte:head><title>Error</title></svelte:head>

<h1 class="adm-h1">Error</h1>
<p class="adm-sub">{page.status}</p>
<div class="card">{page.error?.message}</div>
<a class="btn btn--ghost" href="/" style="margin-top:var(--space)">Back to the overview</a>
