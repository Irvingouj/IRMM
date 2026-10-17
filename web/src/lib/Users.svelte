<script>
  import { onMount } from 'svelte';
  import { api } from '../api.js';

  let users = [];
  let email = '';
  let password = '';
  let role = 'viewer';
  let error = '';

  async function load() {
    users = await api.get('/api/users');
  }
  onMount(() => load().catch((e) => (error = e.message)));

  async function add() {
    error = '';
    try {
      await api.post('/api/users', { email, password, role, client_ids: [] });
      email = '';
      password = '';
      await load();
    } catch (e) {
      error = e.message;
    }
  }
</script>

<h1>Users</h1>
{#if error}<p class="err">{error}</p>{/if}
<div class="card row">
  <label style="margin:0"><span>Email</span><input aria-label="Email" bind:value={email} /></label>
  <label style="margin:0"><span>Password</span><input aria-label="Password" type="password" bind:value={password} /></label>
  <label style="margin:0">
    <span>Role</span>
    <select aria-label="Role" bind:value={role}>
      <option value="viewer">viewer</option>
      <option value="technician">technician</option>
      <option value="admin">admin</option>
      <option value="owner">owner</option>
    </select>
  </label>
  <button on:click={add} disabled={!email || !password}>Add user</button>
</div>
<table>
  <thead><tr><th>Email</th><th>Role</th><th>TOTP</th></tr></thead>
  <tbody>
    {#each users as u (u.id)}
      <tr><td>{u.email}</td><td>{u.role}</td><td>{u.totp_enabled ? 'yes' : 'no'}</td></tr>
    {/each}
  </tbody>
</table>
