<script>
  import { api, authedUrl } from '../api.js';

  export let device;
  let path = '';
  let entries = [];
  let error = '';
  let busy = false;

  const sep = () => (path.includes('\\') ? '\\' : '/');
  const join = (dir, name) => (dir.endsWith(sep()) ? dir + name : dir + sep() + name);

  async function list() {
    error = '';
    try {
      entries = await api.get(`/api/devices/${device}/files?path=${encodeURIComponent(path)}`);
    } catch (e) {
      error = e.message;
    }
  }

  async function upload(e) {
    const file = e.target.files?.[0];
    if (!file) return;
    busy = true;
    error = '';
    try {
      await api.post(`/api/devices/${device}/files/upload?path=${encodeURIComponent(join(path, file.name))}`, file);
      await list();
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
      e.target.value = '';
    }
  }

  function download(entry) {
    window.location.href = authedUrl(`/api/devices/${device}/files/download?path=${encodeURIComponent(join(path, entry.name))}`);
  }
</script>

<div class="card">
  <div class="row">
    <label style="margin:0;flex:1"><span>Path</span><input aria-label="Path" bind:value={path} style="width:100%" /></label>
    <button on:click={list}>Open</button>
    <label style="margin:0">
      <span>Upload</span>
      <input aria-label="Upload" type="file" on:change={upload} disabled={busy} />
    </label>
  </div>
  {#if error}<p class="err">{error}</p>{/if}
  <table>
    <thead><tr><th>Name</th><th>Size</th><th></th></tr></thead>
    <tbody>
      {#each entries as e (e.name)}
        <tr>
          <td>{e.is_dir ? '📁 ' : ''}{e.name}</td>
          <td>{e.is_dir ? '' : e.size}</td>
          <td>{#if !e.is_dir}<button class="secondary" on:click={() => download(e)}>Download</button>{/if}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</div>
