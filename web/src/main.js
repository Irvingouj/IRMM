import { mount } from 'svelte';
import './rtc-shim.js';
import './app.css';
import App from './App.svelte';

export default mount(App, { target: document.getElementById('app') });
