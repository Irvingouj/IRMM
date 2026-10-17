<script>
  import { api, setToken } from '../api.js';
  import { loadMe } from '../store.js';

  let mode = 'login'; // login | signup
  let company = '';
  let email = '';
  let password = '';
  let step = 'credentials'; // credentials | totp
  let pending = '';
  let secret = '';
  let otpauth = '';
  let code = '';
  let error = '';

  async function submitCredentials() {
    error = '';
    try {
      if (mode === 'signup') {
        await api.post('/api/signup', { tenant_name: company, email, password });
      }
      const r = await api.post('/api/auth/login', { email, password });
      pending = r.pending_token;
      secret = r.totp_secret || '';
      otpauth = r.otpauth_url || '';
      step = 'totp';
    } catch (e) {
      error = e.message;
    }
  }

  async function verify() {
    error = '';
    try {
      const r = await api.post('/api/auth/totp', { pending_token: pending, code });
      setToken(r.token);
      await loadMe();
    } catch (e) {
      error = e.message;
    }
  }
</script>

<div style="max-width:380px;margin:80px auto">
  <h1>IRMM</h1>
  {#if step === 'credentials'}
    <div class="card">
      {#if mode === 'signup'}
        <label><span>Company</span><input aria-label="Company" bind:value={company} /></label>
      {/if}
      <label><span>Email</span><input aria-label="Email" bind:value={email} /></label>
      <label><span>Password</span><input aria-label="Password" type="password" bind:value={password} /></label>
      <button on:click={submitCredentials}>{mode === 'signup' ? 'Create account' : 'Sign in'}</button>
      <p>
        {#if mode === 'login'}
          <a href="#" on:click|preventDefault={() => (mode = 'signup')}>Create an account</a>
        {:else}
          <a href="#" on:click|preventDefault={() => (mode = 'login')}>Back to sign in</a>
        {/if}
      </p>
    </div>
  {:else}
    <div class="card">
      {#if secret}
        <p>Set up your authenticator. Secret:</p>
        <code data-testid="totp-secret">{secret}</code>
        {#if otpauth}<p style="font-size:12px;color:#a0a0a8;word-break:break-all">{otpauth}</p>{/if}
      {:else}
        <p>Enter the 6-digit code from your authenticator.</p>
      {/if}
      <label><span>Code</span><input aria-label="Code" bind:value={code} /></label>
      <button on:click={verify}>Verify</button>
    </div>
  {/if}
  {#if error}<p class="err">{error}</p>{/if}
</div>
