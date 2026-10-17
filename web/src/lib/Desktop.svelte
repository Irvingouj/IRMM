<script>
  import { onMount, onDestroy } from 'svelte';
  import { api, wsBase } from '../api.js';
  import { registerRtc } from '../rtc-shim.js';

  export let device;
  export let transport;
  export let monitors = [];

  // The RDP backend embeds a 6 MB wasm as a data: URI; load it as a static module
  // so the bundler never rewrites that payload. The custom element is defined in
  // index.html and dispatches `ready` as soon as it mounts, so we only render it
  // once Backend is loaded and attach the handler at creation time.
  let Backend = null;
  let displayControl = null;
  let el;
  let ui = null;
  let monitor = 0;
  let status = 'idle';
  let error = '';

  const vendor = (name) => import(/* @vite-ignore */ `${location.origin}/vendor/${name}`);

  onMount(async () => {
    try {
      const rdp = await vendor('iron-remote-desktop-rdp.js');
      await rdp.init('INFO'); // loads the embedded wasm and sets the backend log level
      displayControl = rdp.displayControl;
      await customElements.whenDefined('iron-remote-desktop');
      Backend = rdp.Backend; // renders <iron-remote-desktop>, which then fires `ready`
      status = 'connecting';
    } catch (e) {
      status = 'error';
      error = `load failed: ${e.message || e}`;
      console.error('rdp load failed:', e.message || e, e.stack || '');
    }
  });

  function onReady(e) {
    ui = e.detail.irgUserInteraction;
    connect();
  }

  async function connect() {
    if (!ui) return;
    status = 'connecting';
    error = '';
    try {
      const session = await api.post(`/api/devices/${device}/sessions`, { kind: 'rdp', monitor: Number(monitor) });
      let proxyAddress;
      if (transport === 'webrtc') {
        const key = crypto.randomUUID();
        registerRtc(key, { sessionId: session.session_id, ticket: session.ticket, iceServers: session.ice_servers });
        proxyAddress = `rtc://${key}`;
      } else {
        proxyAddress = `${wsBase()}/api/gw/${device}?ticket=${encodeURIComponent(session.ticket)}`;
      }
      const config = ui
        .configBuilder()
        .withUsername('')
        .withPassword('')
        .withDestination(device)
        .withProxyAddress(proxyAddress)
        .withAuthToken(session.ticket)
        .withExtension(displayControl(true))
        .build();
      const active = await ui.connect(config);
      ui.setVisibility(true);
      status = 'connected';
      // `run()` drives the session I/O loop: it flushes queued input to the wire and
      // pumps graphics. It resolves only when the session ends, so don't await it.
      active.run().catch((e) => {
        status = 'disconnected';
        error = e?.message || String(e);
      });
    } catch (e) {
      status = 'error';
      error = e.message || String(e);
      console.error('rdp connect failed:', error, e);
    }
  }

  function disconnect() {
    try {
      ui?.shutdown();
    } catch {}
    status = 'disconnected';
  }

  onDestroy(disconnect);
</script>

<div class="row">
  <label style="margin:0">
    <span>Monitor</span>
    <select bind:value={monitor} on:change={connect}>
      {#each monitors as m}<option value={m.index}>{m.name || `Monitor ${m.index}`}</option>{/each}
    </select>
  </label>
  <button on:click={() => ui?.ctrlAltDel()}>Ctrl+Alt+Del</button>
  <button class="secondary" on:click={disconnect}>Disconnect</button>
  <span>Status: <span data-testid="rdp-status">{status}</span></span>
</div>
{#if error}<p class="err">{error}</p>{/if}
<div class="card" style="height:600px;position:relative">
  {#if Backend}
    <iron-remote-desktop
      bind:this={el}
      module={Backend}
      on:ready={onReady}
      scale="fit"
      verbose="false"
      flexcenter="true"
      style="width:100%;height:100%"
    ></iron-remote-desktop>
  {/if}
</div>
