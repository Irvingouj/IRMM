// Thin REST client. The console is served from the server origin, so paths are relative.
// The session token (set after TOTP) is stored in localStorage and sent as a bearer.

let token = localStorage.getItem('irmm_token') || null;

export function getToken() {
  return token;
}
export function setToken(t) {
  token = t;
  if (t) localStorage.setItem('irmm_token', t);
  else localStorage.removeItem('irmm_token');
}

async function request(method, path, body) {
  const headers = {};
  if (token) headers.authorization = `Bearer ${token}`;
  let payload;
  if (body instanceof Blob) {
    payload = body;
  } else if (body !== undefined) {
    headers['content-type'] = 'application/json';
    payload = JSON.stringify(body);
  }
  const res = await fetch(path, { method, headers, body: payload });
  const text = await res.text();
  const data = text ? JSON.parse(text) : null;
  if (!res.ok) throw new Error(data?.error || data?.message || `${res.status} ${res.statusText}`);
  return data;
}

export const api = {
  get: (p) => request('GET', p),
  post: (p, b) => request('POST', p, b),
  put: (p, b) => request('PUT', p, b),
  del: (p) => request('DELETE', p),
};

export function wsBase() {
  return location.origin.replace(/^http/, 'ws');
}

/** URL for a GET that downloads a file; the token rides in the query (header is unavailable for a navigation). */
export function authedUrl(path) {
  const u = new URL(path, location.origin);
  if (token) u.searchParams.set('token', token);
  return u.toString();
}
