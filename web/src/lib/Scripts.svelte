<script>
  import { onMount } from 'svelte';
  import { api } from '../api.js';

  let scripts = [];
  let devices = [];
  let name = '';
  let shell = 'sh';
  let body = '';
  let error = '';

  let runScript = null; // script being targeted
  let targets = {}; // device id -> bool
  let job = null;
  let poller = null;

  async function load() {
    [scripts, devices] = await Promise.all([api.get('/api/scripts'), api.get('/api/devices')]);
  }
  onMount(() => load().catch((e) => (error = e.message)));

  async function save() {
    error = '';
    try {
      await api.post('/api/scripts', { name, shell, body });
      name = '';
      body = '';
      await load();
    } catch (e) {
      error = e.message;
    }
  }

  function pickTargets(s) {
    runScript = s;
    targets = {};
    job = null;
  }

  async function run() {
    const device_ids = Object.entries(targets).filter(([, v]) => v).map(([k]) => k);
    error = '';
    try {
      const created = await api.post('/api/jobs', { device_ids, script_id: runScript.id });
      clearInterval(poller);
      poller = setInterval(async () => {
        job = await api.get(`/api/jobs/${created.id}`);
        if (job.runs.every((r) => r.status === 'succeeded' || r.status === 'failed')) clearInterval(poller);
      }, 500);
    } catch (e) {
      error = e.message;
    }
  }
</script>

<h1>Scripts</h1>
{#if error}<p class="err">{error}</p>{/if}
<div class="card">
  <label><span>Name</span><input aria-label="Name" bind:value={name} /></label>
  <label><span>Shell</span>
    <select aria-label="Shell" bind:value={shell}>
      {#each ['sh', 'bash', 'zsh', 'powershell', 'cmd', 'python'] as sh}<option value={sh}>{sh}</option>{/each}
    </select>
  </label>
  <label><span>Body</span><textarea aria-label="Body" bind:value={body} rows="4" style="width:100%;background:#26262c;color:#e8e8e8;border:1px solid #3a3a42;border-radius:6px"></textarea></label>
  <button on:click={save} disabled={!name || !body}>Save script</button>
</div>

<table>
  <thead><tr><th>Name</th><th>Shell</th><th></th></tr></thead>
  <tbody>
    {#each scripts as s (s.id)}
      <tr><td>{s.name}</td><td>{s.shell}</td><td><button on:click={() => pickTargets(s)}>Run</button></td></tr>
    {/each}
  </tbody>
</table>

{#if runScript}
  <div class="card">
    <h3>Run “{runScript.name}” on:</h3>
    {#each devices as d (d.id)}
      <label class="row" style="margin:4px 0">
        <input type="checkbox" data-testid="target-{d.id}" bind:checked={targets[d.id]} disabled={!d.online} />
        {d.hostname || d.id} <span class="pill {d.online ? 'online' : ''}">{d.online ? 'online' : 'offline'}</span>
      </label>
    {/each}
    <button on:click={run}>Run on selected</button>
    {#if job}
      <div data-testid="job-runs" style="margin-top:12px">
        {#each job.runs as r}
          <div class="card">
            <div class="row"><strong>{r.hostname || r.device_id}</strong><span class="pill">{r.status}</span><span>exit {r.exit_code ?? '—'}</span></div>
            {#if r.stdout}<pre>{r.stdout}</pre>{/if}
            {#if r.stderr}<pre class="err">{r.stderr}</pre>{/if}
            {#if r.error}<p class="err">{r.error}</p>{/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>
{/if}
