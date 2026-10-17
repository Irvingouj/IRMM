// The browser console against a real server and agent: every MVP workflow an MSP
// technician performs, through the UI.
import { test, expect } from '@playwright/test';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir, platform } from 'node:os';
import { join } from 'node:path';
import { totp } from './totp.js';

const PASSWORD = 'correct horse battery staple';
const control = (path, body) =>
  fetch(process.env.IRMM_CONTROL + path, body ? { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) } : {}).then((r) => r.json());

test.describe.configure({ mode: 'serial' });

let page;
let deviceId;
let secret;

test.beforeAll(async ({ browser }) => {
  page = await browser.newPage();
  page.on('pageerror', (e) => console.log('pageerror:', e.message));
});

test('sign up with mandatory TOTP', async () => {
  await page.goto(process.env.IRMM_URL);
  await page.getByRole('link', { name: 'Create an account' }).click();
  await page.getByLabel('Company').fill('Acme MSP');
  await page.getByLabel('Email').fill('owner@acme.test');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Create account' }).click();
  // First login enrolls TOTP.
  secret = (await page.getByTestId('totp-secret').textContent()).trim();
  await page.getByLabel('Code').fill(totp(secret));
  await page.getByRole('button', { name: 'Verify' }).click();
  await expect(page.getByRole('heading', { name: 'Devices' })).toBeVisible();
  await expect(page.getByText('owner@acme.test')).toBeVisible();
});

test('create client, site and enrollment token; agent enrolls', async () => {
  await page.getByRole('link', { name: 'Clients' }).click();
  await page.getByPlaceholder('New client').fill('Contoso');
  await page.getByRole('button', { name: 'Add client' }).click();
  await page.getByRole('button', { name: 'Contoso' }).click();
  await page.getByPlaceholder('New site').fill('Head office');
  await page.getByRole('button', { name: 'Add site' }).click();
  await page.getByRole('button', { name: 'Enrollment token' }).click();
  const token = (await page.getByTestId('enroll-token').textContent()).trim();
  await expect(page.getByTestId('install-linux')).toContainText('/install.sh');
  await expect(page.getByTestId('install-windows')).toContainText('/install.ps1');
  ({ device_id: deviceId } = await control('/agents', { token }));

  await page.getByRole('link', { name: 'Devices' }).click();
  const row = page.getByTestId(`device-${deviceId}`);
  await expect(row).toContainText('online');
  await expect(row).toContainText('Contoso');
  await row.getByRole('link').first().click();
  await expect(page.getByTestId('inventory')).toContainText('Memory');
  await expect(page.getByTestId('inventory')).toContainText('640×480');
});

test('terminal', async () => {
  await page.getByRole('button', { name: 'Terminal' }).click();
  const term = page.locator('.xterm');
  await expect(term).toBeVisible();
  await term.click();
  const cmd = platform() === 'win32' ? 'Write-Output ("console-" + (6*7))' : 'echo console-$((6*7))';
  await page.keyboard.type(cmd);
  await page.keyboard.press('Enter');
  await expect(page.locator('.xterm-rows')).toContainText('console-42');
});

async function canvasColor() {
  return page.evaluate(() => {
    const el = document.querySelector('iron-remote-desktop');
    const canvas = el?.shadowRoot?.querySelector('canvas') ?? el?.querySelector('canvas');
    if (!canvas || !canvas.width) return null;
    const c = document.createElement('canvas');
    c.width = canvas.width;
    c.height = canvas.height;
    const ctx = c.getContext('2d');
    ctx.drawImage(canvas, 0, 0);
    return Array.from(ctx.getImageData(Math.floor(canvas.width / 2), Math.floor(canvas.height / 2), 1, 1).data);
  });
}

const near = (px, rgb) => px && rgb.every((v, i) => Math.abs(px[i] - v) <= 16);

for (const transport of ['webrtc', 'gateway']) {
  test(`remote desktop over ${transport}`, async () => {
    await control('/desktop/color', { rgb: transport === 'webrtc' ? [0x20, 0x90, 0xf0] : [0xf0, 0x40, 0x20] });
    await page.getByLabel('Transport').selectOption(transport);
    await page.getByRole('button', { name: 'Desktop' }).click();
    await expect(page.getByTestId('rdp-status')).toHaveText('connected');
    const want = transport === 'webrtc' ? [0x20, 0x90, 0xf0] : [0xf0, 0x40, 0x20];
    await expect.poll(canvasColor, { timeout: 20_000 }).toSatisfy((px) => near(px, want));
    await page.getByRole('button', { name: 'Ctrl+Alt+Del' }).click();
    await expect.poll(async () => (await control('/desktop/events')).filter((e) => e === 'SecureAttention').length).toBeGreaterThan(0);
    await page.getByRole('button', { name: 'Disconnect' }).click();
  });
}

test('files: upload, list, download', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'irmm-console-'));
  const local = join(dir, 'local.txt');
  writeFileSync(local, 'hello from the console');
  await page.getByRole('button', { name: 'Files' }).click();
  await page.getByLabel('Path').fill(dir);
  await page.getByRole('button', { name: 'Open' }).click();
  await page.getByLabel('Upload').setInputFiles({ name: 'uploaded.txt', mimeType: 'text/plain', buffer: Buffer.from('uploaded content') });
  await expect(page.getByRole('row', { name: /uploaded\.txt/ })).toBeVisible();
  const download = page.waitForEvent('download');
  await page.getByRole('row', { name: /local\.txt/ }).getByRole('button', { name: 'Download' }).click();
  expect((await download).suggestedFilename()).toBe('local.txt');
});

test('script library and job output', async () => {
  await page.getByRole('link', { name: 'Scripts' }).click();
  const shell = platform() === 'win32' ? 'powershell' : 'sh';
  await page.getByLabel('Name').fill('Say hello');
  await page.getByLabel('Shell').selectOption(shell);
  await page.getByLabel('Body').fill(platform() === 'win32' ? 'Write-Output "job-says-hi"' : 'echo job-says-hi');
  await page.getByRole('button', { name: 'Save script' }).click();
  await page.getByRole('row', { name: /Say hello/ }).getByRole('button', { name: 'Run' }).click();
  await page.getByTestId(`target-${deviceId}`).check();
  await page.getByRole('button', { name: 'Run on selected' }).click();
  await expect(page.getByTestId('job-runs')).toContainText('succeeded');
  await expect(page.getByTestId('job-runs')).toContainText('job-says-hi');
});

test('users and audit log', async () => {
  await page.getByRole('link', { name: 'Users' }).click();
  await page.getByLabel('Email').fill('tech@acme.test');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByLabel('Role').selectOption('technician');
  await page.getByRole('button', { name: 'Add user' }).click();
  await expect(page.getByRole('row', { name: /tech@acme\.test/ })).toContainText('technician');

  await page.getByRole('link', { name: 'Audit' }).click();
  await expect(page.getByRole('table')).toContainText('session.create');
  await expect(page.getByRole('table')).toContainText('device.enroll');
  await expect(page.getByRole('table')).toContainText('user.create');
});
