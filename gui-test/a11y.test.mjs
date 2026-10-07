// #191 stage 1: accessibility + Vanilla-adoption invariants, checked in
// jsdom against the real index.html/app.js: the invoice chart's accessible
// name carries the per-month summary (role="img" hides its subtree), the
// contact editor and invoice-line inputs have real accessible names, and
// explicit data-icon/data-positive drive the Ubuntu chrome even when labels
// no longer match the old text heuristics.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { JSDOM } from 'jsdom';

const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const app = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

function setup() {
  const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
  dom.window.HTMLDialogElement.prototype.showModal = function () { this.open = true; };
  dom.window.HTMLDialogElement.prototype.close = function () { this.open = false; };
  dom.window.eval(`${app}
window.t = {
  dashRenderOverview, state, api,
  contactRow, edLineRow, decorate: decorateUbuntuButton,
};`);
  return { dom, t: dom.window.t, document: dom.window.document };
}

test('invoice chart exposes its numbers through the accessible name (#191)', () => {
  const { dom, t, document } = setup();
  t.dashRenderOverview([
    {
      id: 'i1', number: 'INV-1', status: 'issued', currency: 'EUR',
      period_to: '2026-03-10', total_minor: 180000, payments: [],
    },
  ]);
  const chart = document.getElementById('inv-chart');
  assert.equal(chart.getAttribute('role'), 'img');
  const label = chart.getAttribute('aria-label') || '';
  assert.match(label, /open 1800\.00/, `summary must ride the name: ${label}`);
  // Children of role="img" are presentational — the old sr-only child is gone.
  assert.equal(chart.querySelector('.sr-only'), null,
    'no DOM summary inside a role="img" subtree');
  dom.window.close();
});

test('contact editor inputs have accessible names (#191)', () => {
  const { dom, t, document } = setup();
  const row = t.contactRow();
  document.body.appendChild(row);
  for (const input of row.querySelectorAll('input[type="text"], input[type="email"]')) {
    assert.ok(input.getAttribute('aria-label'), `aria-label required on ${input.placeholder}`);
  }
  dom.window.close();
});

test('invoice-line inputs have accessible names (#191)', () => {
  const { dom, t, document } = setup();
  const row = t.edLineRow();
  document.body.appendChild(row);
  const named = [...row.querySelectorAll('input, select')]
    .map((n) => n.getAttribute('aria-label') || n.getAttribute('placeholder') || '');
  for (const want of ['Line description', 'Quantity', 'Unit price', 'Product or service']) {
    assert.ok(named.includes(want), `missing accessible name: ${want}`);
  }
  dom.window.close();
});

test('data-icon/data-positive decorate buttons independently of label text (#191)', () => {
  const { dom, t, document } = setup();
  const button = document.createElement('button');
  // A renamed label ("Generate weekly statement") would have lost every
  // heuristic icon under the old text-matching; data-icon keeps it stable.
  button.dataset.icon = 'history';
  button.dataset.positive = '1';
  button.textContent = 'Generate weekly statement';
  document.body.appendChild(button);
  t.decorate(button);
  assert.ok(button.querySelector('[class*="p-icon--history"]'), 'explicit icon applied');
  assert.ok(button.classList.contains('p-button--positive'), 'explicit positive applied');
  dom.window.close();
});

test('the wizard button keeps its plus glyph without the label prefix (#191)', () => {
  const { dom, t, document } = setup();
  const button = document.getElementById('customer-new');
  t.decorate(button);
  assert.ok(button.querySelector('[class*="p-icon--plus"]'), 'plus icon from data-icon');
  assert.equal(button.textContent, 'Customer', 'label text unchanged by decoration');
  dom.window.close();
});
