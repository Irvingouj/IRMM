<script>
  import { onMount } from 'svelte';
  import { api, authedUrl } from '../api.js';

  let clients = [];
  let sitesByClient = {};
  let openClient = null;
  let newClient = '';
  let newSite = '';
  let token = null;
  let error = '';

  async function load() {
    clients = await api.get('/api/clients');
  }
  onMount(() => load().catch((e) => (error = e.message)));

  async function addClient() {
    await api.post('/api/clients', { name: newClient });
    newClient = '';
    await load();
  }
  async function selectClient(c) {
    openClient = c;
    token = null;
    sitesByClient[c.id] = await api.get(`/api/clients/${c.id}/sites`);
  }
  async function addSite() {
    await api.post(`/api/clients/${openClient.id}/sites`, { name: newSite });
    newSite = '';
    await selectClient(openClient);
  }
  async function setMesh(c, enabled) {
    try {
      Object.assign(c, await api.put(`/api/clients/${c.id}/mesh`, { enabled }));
      clients = clients;
    } catch (e) {
      error = e.message;
    }
  }
  async function makeToken(site) {
    token = await api.post(`/api/sites/${site.id}/enroll-tokens`, { max_uses: 10, ttl_hours: 24 });
  }
</script>

<h1>Clients</h1>
{#if error}<p class="err">{error}</p>{/if}
<div class="row">
  <input placeholder="New client" bind:value={newClient} />
  <button on:click={addClient} disabled={!newClient}>Add client</button>
</div>

{#each clients as c (c.id)}
  <div class="card">
    <div class="row">
      <button class="secondary" on:click={() => selectClient(c)}>{c.name}</button>
      <label class="row" style="margin:0">
        <input type="checkbox" checked={c.mesh_enabled} on:change={(e) => setMesh(c, e.currentTarget.checked)} />
        Mesh VPN
      </label>
      {#if c.mesh_enabled}
        <span data-testid="mesh-range">{c.mesh_ipv4_range} · {c.mesh_ipv6_prefix}</span>
      {/if}
    </div>
    {#if openClient?.id === c.id}
      <div style="margin-top:12px">
        <div class="row">
          <input placeholder="New site" bind:value={newSite} />
          <button on:click={addSite} disabled={!newSite}>Add site</button>
        </div>
        {#each sitesByClient[c.id] || [] as s (s.id)}
          <div class="row" style="margin-top:8px">
            <strong>{s.name}</strong>
            <button on:click={() => makeToken(s)}>Enrollment token</button>
          </div>
        {/each}
        {#if token}
          <div class="card">
            <p>Token: <code data-testid="enroll-token">{token.token}</code></p>
            <p>Linux / macOS:</p>
            <pre data-testid="install-linux">{token.install.linux}</pre>
            <p>Windows:</p>
            <pre data-testid="install-windows">{token.install.windows}</pre>
          </div>
        {/if}
      </div>
    {/if}
  </div>
{/each}
