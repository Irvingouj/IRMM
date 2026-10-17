<script>
  import { onMount, onDestroy } from 'svelte';
  import { Terminal } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import '@xterm/xterm/css/xterm.css';
  import { openFramedChannel } from '../transport.js';

  export let device;
  export let transport;

  let el;
  let term;
  let chan = null;
  let queue = []; // keystrokes typed before the channel is ready
  // The protocol uses URL-safe base64 without padding (matches the Rust agent).
  const b64 = (bytes) => btoa(String.fromCharCode(...bytes)).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
  const unb64 = (s) => {
    const b = s.replaceAll('-', '+').replaceAll('_', '/').padEnd(Math.ceil(s.length / 4) * 4, '=');
    return Uint8Array.from(atob(b), (c) => c.charCodeAt(0));
  };

  function sendData(d) {
    const frame = { type: 'data', data: b64(new TextEncoder().encode(d)) };
    if (chan) chan.write(frame);
    else queue.push(frame);
  }

  onMount(async () => {
    term = new Terminal({ fontSize: 13, theme: { background: '#1b1b1f' } });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);
    fit.fit();
    term.focus();
    // Wire input before connecting so keystrokes are buffered, not lost.
    term.onData(sendData);
    term.onResize(({ cols, rows }) => chan?.write({ type: 'resize', cols, rows }));

    chan = await openFramedChannel({ device, kind: 'shell', transport });
    chan.onMessage((m) => {
      if (m.type === 'data') term.write(unb64(m.data));
      else if (m.type === 'exit') term.write(`\r\n[exited: ${m.code ?? '?'}]\r\n`);
    });
    chan.onClose(() => term.write('\r\n[disconnected]\r\n'));
    chan.write({ type: 'start', shell: null, cols: term.cols, rows: term.rows });
    for (const f of queue) chan.write(f);
    queue = [];
  });

  onDestroy(() => {
    chan?.close();
    term?.dispose();
  });
</script>

<div class="card"><div bind:this={el} style="height:420px"></div></div>
