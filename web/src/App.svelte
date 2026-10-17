<script>
  import { onMount } from 'svelte';
  import { me, nav, openDevice, loadMe, logout } from './store.js';
  import Auth from './lib/Auth.svelte';
  import Devices from './lib/Devices.svelte';
  import Device from './lib/Device.svelte';
  import Clients from './lib/Clients.svelte';
  import Scripts from './lib/Scripts.svelte';
  import Users from './lib/Users.svelte';
  import Audit from './lib/Audit.svelte';

  onMount(loadMe);

  function go(page) {
    openDevice.set(null);
    nav.set(page);
  }
</script>

{#if !$me}
  <Auth />
{:else}
  <header class="row" style="padding:12px 20px;border-bottom:1px solid #2d2d34;justify-content:space-between">
    <nav>
      <strong style="margin-right:20px">IRMM</strong>
      <a href="#" on:click|preventDefault={() => go('devices')}>Devices</a>
      <a href="#" on:click|preventDefault={() => go('clients')}>Clients</a>
      <a href="#" on:click|preventDefault={() => go('scripts')}>Scripts</a>
      <a href="#" on:click|preventDefault={() => go('users')}>Users</a>
      <a href="#" on:click|preventDefault={() => go('audit')}>Audit</a>
    </nav>
    <div class="row">
      <span>{$me.email}</span>
      <span class="pill">{$me.role}</span>
      <button class="secondary" on:click={logout}>Sign out</button>
    </div>
  </header>
  <main style="padding:20px;max-width:1100px;margin:0 auto">
    {#if $openDevice}
      <Device id={$openDevice} />
    {:else if $nav === 'devices'}
      <Devices />
    {:else if $nav === 'clients'}
      <Clients />
    {:else if $nav === 'scripts'}
      <Scripts />
    {:else if $nav === 'users'}
      <Users />
    {:else if $nav === 'audit'}
      <Audit />
    {/if}
  </main>
{/if}
