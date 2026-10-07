// #182: the Setup workspace split — three sections (Customers / Projects /
// Tasks), customer-scoped project filter, dependent customer+project filters
// on tasks, dependent resets and accessible empty/announced states. Offline
// jsdom against the real index.html + app.js.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { JSDOM } from 'jsdom';

const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const app = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

const customers = [
  { id: 'c1', name: 'ACME', currency: 'EUR', default_rate_minor: 6000, active: true, email: '' },
  { id: 'c2', name: 'Globex', currency: 'USD', default_rate_minor: 4550, active: true, email: '' },
];
const projectsByCustomer = {
  c1: [
    { code: 'ACM', name: 'Web', currency: 'EUR', rate_minor: 6000, active: true },
    { code: 'MKT-2', name: 'Marketing', currency: 'EUR', rate_minor: 9500, active: true },
  ],
  c2: [
    { code: 'GX-1', name: 'Labs', currency: 'USD', rate_minor: 4550, active: true },
  ],
};
const tasksByProject = {
  'c1/MKT-2': [
    { code: 'W-1', name: 'Weekly', active: true, customer_id: 'c1', project_code: 'MKT-2' },
  ],
};

function setup() {
  const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
  dom.window.HTMLDialogElement.prototype.showModal = function () { this.open = true; };
  dom.window.HTMLDialogElement.prototype.close = function () { this.open = false; };
  const stub = `
api.get = async (path) => {
  if (path === '/customers') return { customers: ${JSON.stringify(customers)} };
  let m = path.match(/^\\/customers\\/([^/]+)\\/projects$/);
  if (m) return { projects: ${JSON.stringify(projectsByCustomer)}[m[1]] || [] };
  m = path.match(/^\\/customers\\/([^/]+)\\/projects\\/([^/]+)\\/tasks$/);
  if (m) return { tasks: ${JSON.stringify(tasksByProject)}[m[1] + '/' + decodeURIComponent(m[2])] || [] };
  return {};
};
initTabs();
window.h = {
  state, openProjectsFor, openTasksFor, refreshProjectTable, refreshTaskTable,
  switchTab, api,
};`;
  dom.window.eval(`${app}\n${stub}`);
  return { dom, h: dom.window.h, document: dom.window.document };
}

const tick = (ms = 60) => new Promise((done) => setTimeout(done, ms));

test('Setup exposes three sibling sections with tab/panel wiring (#182)', () => {
  const { dom, document } = setup();
  const tabs = ['tab-customers', 'tab-projects', 'tab-tasks']
    .map((id) => document.getElementById(id));
  assert.ok(tabs.every((t) => t && t.getAttribute('role') === 'tab'), 'three tab buttons');
  const panels = tabs.map((t) => document.getElementById(t.getAttribute('aria-controls')));
  assert.ok(panels.every((p) => p && p.getAttribute('role') === 'tabpanel'), 'matching panels');
  assert.deepEqual(
    [...document.querySelectorAll('#tabs .nav-group')].flatMap((g) =>
      [...g.querySelectorAll('[role="tab"]')].map((t) => t.id)),
    ['tab-timesheet', 'tab-customers', 'tab-projects', 'tab-tasks',
      'tab-invoices', 'tab-expenses', 'tab-submissions', 'tab-reports', 'tab-settings'],
    'Customers/Projects/Tasks sit together in the Setup group',
  );
  dom.window.close();
});

test('section navigation shows one panel at a time and scopes project rows (#182)', async () => {
  const { dom, h, document } = setup();
  await h.openProjectsFor(null); // all customers
  await tick();
  assert.equal(document.getElementById('panel-projects').hidden, false);
  assert.equal(document.getElementById('panel-customers').hidden, true);
  const allRows = [...document.querySelectorAll('#project-table tbody tr')];
  assert.equal(allRows.length, 3, 'all customers by default');
  assert.equal(allRows[0].cells[1].textContent, 'ACME', 'Customer column present');
  // Cross-navigation from the Customers section filters the Projects section.
  await h.openProjectsFor('c2');
  await tick();
  const scoped = [...document.querySelectorAll('#project-table tbody tr')];
  assert.equal(scoped.length, 1);
  assert.equal(scoped[0].cells[0].textContent.trim(), 'GX-1');
  assert.match(document.getElementById('project-scope-label').textContent, /Globex/);
  assert.equal(document.getElementById('proj-filter-customer').value, 'c2', 'filter select reflects scope');
  // Clearing the filter returns to the all-customers view (#182 DoD).
  const sel = document.getElementById('proj-filter-customer');
  sel.value = '';
  sel.dispatchEvent(new dom.window.Event('change', { bubbles: true }));
  await tick();
  assert.equal(document.querySelectorAll('#project-table tbody tr').length, 3);
  assert.match(document.getElementById('live-region').textContent, /all customers/i);
  dom.window.close();
});

test('tasks filters are dependent and the project choices follow the customer (#182)', async () => {
  const { dom, h, document } = setup();
  await h.openTasksFor('c1', 'MKT-2');
  await tick();
  assert.equal(document.getElementById('panel-tasks').hidden, false);
  assert.equal(document.getElementById('task-filter-customer').value, 'c1');
  const proj = document.getElementById('task-filter-project');
  assert.equal(proj.disabled, false, 'project filter enabled once a customer is picked');
  assert.deepEqual(
    [...proj.options].map((o) => o.value),
    ['', 'ACM', 'MKT-2'],
    'project choices are scoped to the selected customer only',
  );
  assert.equal(proj.value, 'MKT-2');
  assert.equal(document.querySelector('#task-table tbody tr th').textContent, 'W-1');
  assert.equal(document.getElementById('tasks-empty-state').hidden, true);

  // Clearing the customer resets the dependent project pick and shows the
  // accessible empty state (#182 DoD).
  const cust = document.getElementById('task-filter-customer');
  cust.value = '';
  cust.dispatchEvent(new dom.window.Event('change', { bubbles: true }));
  await tick();
  assert.equal(proj.value, '', 'child selection reset with the parent');
  assert.equal(proj.disabled, true, 'project filter disabled without a customer');
  assert.equal(document.getElementById('task-table').hidden, true);
  assert.equal(document.getElementById('tasks-empty-state').hidden, false);
  assert.match(document.getElementById('live-region').textContent, /cleared/i);
  dom.window.close();
});

test('tasks list filters to the selected project and announces (#182)', async () => {
  const { dom, h, document } = setup();
  await h.openTasksFor('c1', '');
  await tick();
  // Customer only: no task list yet — empty state explains the next step.
  assert.match(document.getElementById('tasks-empty-state').textContent, /choose a project/i);
  const proj = document.getElementById('task-filter-project');
  proj.value = 'MKT-2';
  proj.dispatchEvent(new dom.window.Event('change', { bubbles: true }));
  await tick();
  assert.equal(document.querySelector('#task-table tbody tr th').textContent, 'W-1');
  assert.match(document.getElementById('live-region').textContent, /ACME \/ MKT-2/);
  dom.window.close();
});

test('refreshProjectTable is the single owner of the filter select value (#219)', async () => {
  // saveProject re-scopes state without touching the select; the render must
  // still leave the control truthful (before #219 it showed a stale value).
  const { dom, h, document } = setup();
  await h.openProjectsFor('c1');
  await tick();
  h.state.projectsFilter = 'c2'; // what saveProject does after saving elsewhere
  await h.refreshProjectTable();
  await tick();
  assert.equal(document.getElementById('proj-filter-customer').value, 'c2',
    'select re-syncs from state on every render');
  assert.equal(document.querySelectorAll('#project-table tbody tr').length, 1);
  dom.window.close();
});
