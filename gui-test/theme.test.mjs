import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { JSDOM } from 'jsdom';

const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const script = readFileSync(new URL('../web/theme.js', import.meta.url), 'utf8');

function setup({ dark = false, saved, blocked = false } = {}) {
  const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
  let listener;
  dom.window.matchMedia = () => ({ matches: dark, addEventListener: (_event, handler) => { listener = handler; } });
  if (saved) dom.window.localStorage.setItem('tucanotime-colour-mode', saved);
  if (blocked) Object.defineProperty(dom.window, 'localStorage', { get() { throw new Error('Unavailable'); } });
  dom.window.eval(script);
  dom.window.document.dispatchEvent(new dom.window.Event('DOMContentLoaded'));
  return { dom, changeSystem: (matches) => listener({ matches }) };
}

test('switch is below wizard and outside navigation tablist', () => {
  const { dom } = setup();
  const control = dom.window.document.getElementById('theme-switch');
  assert.equal(control.previousElementSibling.id, 'tabs');
  assert.equal(dom.window.document.querySelector('#tabs > button:last-child').id, 'wizard-open');
  assert.equal(control.closest('[role="tablist"]'), null);
  dom.window.close();
});

test('default follows system until manual selection', () => {
  const { dom, changeSystem } = setup({ dark: true });
  const document = dom.window.document;
  assert.equal(document.documentElement.dataset.theme, 'dark');
  assert.equal(document.body.classList.contains('is-dark'), true);
  changeSystem(false);
  assert.equal(document.documentElement.dataset.theme, 'light');
  const dark = document.querySelector('input[value="dark"]');
  dark.checked = true;
  dark.dispatchEvent(new dom.window.Event('change'));
  changeSystem(false);
  assert.equal(document.documentElement.dataset.theme, 'dark');
  assert.equal(dom.window.localStorage.getItem('tucanotime-colour-mode'), 'dark');
  dom.window.close();
});

test('saved light choice overrides dark system on reload', () => {
  const { dom } = setup({ dark: true, saved: 'light' });
  assert.equal(dom.window.document.documentElement.dataset.theme, 'light');
  assert.equal(dom.window.document.querySelector('input[value="light"]').checked, true);
  dom.window.close();
});

test('blocked storage does not prevent switching', () => {
  const { dom } = setup({ blocked: true });
  const input = dom.window.document.querySelector('input[value="dark"]');
  input.checked = true;
  input.dispatchEvent(new dom.window.Event('change'));
  assert.equal(dom.window.document.documentElement.dataset.theme, 'dark');
  dom.window.close();
});