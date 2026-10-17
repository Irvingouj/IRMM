// Builds the console, then starts a real IRMM server + agent control endpoint (the
// `devstack` binary) and exposes its URLs to the tests.
import { spawn, execSync } from 'node:child_process';
import { createInterface } from 'node:readline';

export default async function globalSetup() {
  execSync('npm run build', { stdio: 'inherit' });
  const proc = spawn('cargo', ['run', '-q', '-p', 'irmm-e2e', '--bin', 'devstack'], {
    cwd: new URL('../..', import.meta.url).pathname,
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  const line = await new Promise((resolve, reject) => {
    const rl = createInterface({ input: proc.stdout });
    rl.once('line', resolve);
    proc.once('exit', (code) => reject(new Error(`devstack exited with ${code}`)));
  });
  const { url, control } = JSON.parse(line);
  process.env.IRMM_URL = url;
  process.env.IRMM_CONTROL = control;
  return () => proc.kill();
}
