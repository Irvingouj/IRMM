<script>
  import { onMount } from 'svelte';
  import { api } from '../api.js';

  let rows = [];
  let error = '';
  onMount(async () => {
    try {
      rows = await api.get('/api/audit');
    } catch (e) {
      error = e.message;
    }
  });
</script>

<h1>Audit log</h1>
{#if error}<p class="err">{error}</p>{/if}
<table>
  <thead><tr><th>When</th><th>Actor</th><th>Action</th><th>Target</th></tr></thead>
  <tbody>
    {#each rows as r (r.id)}
      <tr><td>{r.at}</td><td>{r.actor_email || '—'}</td><td>{r.action}</td><td>{r.target || ''}</td></tr>
    {/each}
  </tbody>
</table>
