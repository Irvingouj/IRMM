<script>
  import { onMount } from 'svelte';
  import { api } from '../api.js';
  import { openDevice } from '../store.js';

  let devices = [];
  let error = '';

  async function load() {
    try {
      devices = await api.get('/api/devices');
    } catch (e) {
      error = e.message;
    }
  }
  onMount(load);
</script>

<h1>Devices</h1>
{#if error}<p class="err">{error}</p>{/if}
<table>
  <thead>
    <tr><th>Host</th><th>Client / Site</th><th>OS</th><th>Mesh IP</th><th>Status</th></tr>
  </thead>
  <tbody>
    {#each devices as d (d.id)}
      <tr data-testid="device-{d.id}">
        <td><a href="#" on:click|preventDefault={() => openDevice.set(d.id)}>{d.hostname || d.id}</a></td>
        <td>{d.client_name} / {d.site_name}</td>
        <td>{d.os} {d.os_version}</td>
        <td>{d.mesh_ipv4 || ''}</td>
        <td><span class="pill {d.online ? 'online' : ''}">{d.online ? 'online' : 'offline'}</span></td>
      </tr>
    {/each}
  </tbody>
</table>
{#if devices.length === 0}<p>No devices yet. Create an enrollment token under Clients.</p>{/if}
