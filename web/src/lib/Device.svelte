<script>
  import { onMount } from 'svelte';
  import { api } from '../api.js';
  import { openDevice } from '../store.js';
  import Terminal from './Terminal.svelte';
  import Desktop from './Desktop.svelte';
  import Files from './Files.svelte';

  export let id;
  let device = null;
  let tab = null; // terminal | desktop | files
  let transport = 'webrtc';
  let error = '';

  function fmtBytes(n) {
    if (!n) return '0 B';
    const u = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(n) / Math.log(1024));
    return `${(n / 1024 ** i).toFixed(1)} ${u[i]}`;
  }

  onMount(async () => {
    try {
      device = await api.get(`/api/devices/${id}`);
    } catch (e) {
      error = e.message;
    }
  });
</script>

<p><a href="#" on:click|preventDefault={() => openDevice.set(null)}>&larr; Devices</a></p>
{#if error}<p class="err">{error}</p>{/if}
{#if device}
  <h1>{device.hostname || device.id}</h1>
  <div class="row">
    <span class="pill {device.online ? 'online' : ''}">{device.online ? 'online' : 'offline'}</span>
    <span>{device.os} {device.os_version} · {device.arch}</span>
  </div>

  {#if device.mesh_ipv4}
    <div class="card" data-testid="mesh">
      <div class="row"><strong>Mesh IPv4</strong><span>{device.mesh_ipv4}</span></div>
      <div class="row"><strong>Mesh IPv6</strong><span>{device.mesh_ipv6}</span></div>
    </div>
  {/if}

  <div class="card" data-testid="inventory">
    {#if device.inventory}
      {@const inv = device.inventory}
      <div class="row"><strong>CPU</strong><span>{inv.cpu_model} ({inv.cpu_cores} cores)</span></div>
      <div class="row"><strong>Memory</strong><span>{fmtBytes(inv.memory_bytes)}</span></div>
      <div class="row"><strong>IPs</strong><span>{(inv.ips || []).join(', ')}</span></div>
      <div class="row"><strong>Users</strong><span>{(inv.logged_in_users || []).join(', ')}</span></div>
      <div class="row">
        <strong>Monitors</strong>
        <span>{(inv.monitors || []).map((m) => `${m.width}×${m.height}`).join(', ')}</span>
      </div>
      <div><strong>Disks</strong>
        <ul>
          {#each inv.disks || [] as d}
            <li>{d.mount}: {fmtBytes(d.available_bytes)} free of {fmtBytes(d.total_bytes)}</li>
          {/each}
        </ul>
      </div>
    {/if}
  </div>

  <div class="row">
    <button on:click={() => (tab = 'terminal')}>Terminal</button>
    <button on:click={() => (tab = 'desktop')}>Desktop</button>
    <button on:click={() => (tab = 'files')}>Files</button>
    <label style="margin:0">
      <span>Transport</span>
      <select aria-label="Transport" bind:value={transport}>
        <option value="webrtc">webrtc</option>
        <option value="gateway">gateway</option>
      </select>
    </label>
  </div>

  {#if tab === 'terminal'}
    <Terminal device={id} {transport} />
  {:else if tab === 'desktop'}
    {#key transport}
      <Desktop device={id} monitors={device.inventory?.monitors || []} {transport} />
    {/key}
  {:else if tab === 'files'}
    <Files device={id} {transport} />
  {/if}
{/if}
