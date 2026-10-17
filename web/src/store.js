import { writable } from 'svelte/store';
import { api, setToken, getToken } from './api.js';

export const me = writable(null);
export const nav = writable('devices'); // devices | clients | scripts | users | audit
export const openDevice = writable(null); // device id or null

export async function loadMe() {
  if (!getToken()) {
    me.set(null);
    return;
  }
  try {
    me.set(await api.get('/api/me'));
  } catch {
    setToken(null);
    me.set(null);
  }
}

export function logout() {
  api.post('/api/auth/logout').catch(() => {});
  setToken(null);
  me.set(null);
}
