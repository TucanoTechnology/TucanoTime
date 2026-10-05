// Headless GUI smoke test: load the real index.html + app.js into jsdom,
// wire fetch to the live server, run boot(), then simulate adding an entry
// and creating a customer through the actual forms. Verifies the presentation
// layer drives the API and renders results.

import { JSDOM, VirtualConsole } from 'jsdom';
import fs from 'node:fs';

const BASE = 'http://localhost:8099';

const html = fs.readFileSync('../web/index.html', 'utf8');
const appJs = fs.readFileSync('../web/app.js', 'utf8');

const vc = new VirtualConsole();
vc.on('jsdomError', (e) => console.error('[jsdom error]', e.message));
vc.on('error', (m) => console.error('[console.error]', m));

const dom = new JSDOM(html, { runScripts: 'outside-only', pretendToBeVisual: true, url: BASE + '/', virtualConsole: vc });
const { window } = dom;

// Cookie-aware fetch for the whole harness: attaches the session once logged in.
const _fetch = globalThis.fetch.bind(globalThis);
let SESSION_COOKIE = '';
function fetch(url, init = {}) {
  const headers = { ...(init.headers || {}) };
  if (SESSION_COOKIE && !('Cookie' in headers)) headers.Cookie = SESSION_COOKIE;
  headers['X-CSRF-Protection'] = '1';
  return _fetch(url, { ...init, headers });
}

// window.fetch resolves root-relative paths against BASE and uses the wrapper.
window.fetch = (input, init = {}) => {
  const url = typeof input === 'string' && input.startsWith('/') ? BASE + input : input;
  return fetch(url, init);
};
window.confirm = () => true;
window.alert = () => {};
if (!window.Element.prototype.scrollIntoView) window.Element.prototype.scrollIntoView = () => {};

// --- authenticate + seed against the live server (fresh data dir) ---
const postJson = (path, body) =>
  fetch(path.startsWith('http') ? path : BASE + path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
await postJson(BASE + '/auth/bootstrap', { name: 'Admin', email: 'admin@test.local', password: 'supersecret1' });
const loginRes = await postJson(BASE + '/auth/login', { email: 'admin@test.local', password: 'supersecret1' });
SESSION_COOKIE = (loginRes.headers.get('set-cookie') || '').split(';')[0];
// Seed a customer + project (authenticated via the wrapper).
const cust = await (
  await postJson('/customers', { name: 'ACME', currency: 'EUR', default_rate_minor: 6000 })
).json();
await postJson(`/customers/${cust.id}/projects`, { code: 'P-9', name: 'Portal', currency: 'EUR', rate_minor: 6000 });

function tick(ms = 60) { return new Promise((r) => setTimeout(r, ms)); }

// Inject and run the real app, then fire DOMContentLoaded.
window.eval(appJs);
window.document.dispatchEvent(new window.Event('DOMContentLoaded', { bubbles: true }));
await tick(200);

let failures = 0;
function check(name, cond) {
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}`);
  if (!cond) failures++;
}

// ---- boot rendered the customer picker with the seeded ACME ----
const customerSelect = window.document.getElementById('entry-customer');
check('customer picker populated', [...customerSelect.options].some((o) => o.textContent.includes('ACME')));

// ---- pick ACME, load its project, fill the entry form and submit ----
const acmeOpt = [...customerSelect.options].find((o) => o.textContent.includes('ACME'));
customerSelect.value = acmeOpt.value;
customerSelect.dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(150);

const projectSelect = window.document.getElementById('entry-project');
const p9 = [...projectSelect.options].find((o) => o.value === 'P-9');
check('project picker loaded P-9 from API', !!p9);
projectSelect.value = 'P-9';

window.document.getElementById('entry-date').value = '2026-11-11';
window.document.getElementById('entry-hours').value = '4.25';
window.document.getElementById('entry-note').value = 'gui-test entry';

const entryForm = window.document.getElementById('entry-form');
entryForm.dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);

// ---- verify the entry actually reached the server through the form ----
const day = await (await fetch(BASE + '/entries?date=2026-11-11')).json();
const made = day.entries.find((e) => e.hours === 4.25 && e.project_code === 'P-9');
check('form submit created the entry via API', !!made);
check('entry note stored as typed', made && made.note === 'gui-test entry');

// ---- day table re-rendered and shows the new row ----
const rows = window.document.querySelectorAll('#day-table tbody tr');
check('day table renders the new entry row', rows.length >= 1);
const totals = window.document.getElementById('day-total').textContent;
check('day total reflects 4.25h', totals.trim() === '4.25');

// ---- live region announced the success (accessibility) ----
const live = window.document.getElementById('live-region').textContent;
check('aria-live region announced an add', /added|updated/i.test(live));

// ---- DAY REDESIGN (#12): row structure, week strip, navigator, copy-forward ----
const mainCell = rows[0].querySelector('.entry-main');
check(
  'entry row shows project / customer / note lines',
  mainCell.textContent.includes('P-9') && mainCell.textContent.includes('ACME') && mainCell.textContent.includes('gui-test entry'),
);
const stripDays = window.document.querySelectorAll('#week-strip .ws-day');
check('week strip renders seven days', stripDays.length === 7);
const selDay = window.document.querySelector('#week-strip .ws-day.selected');
check('selected day emphasised with H:MM total', selDay && selDay.dataset.date === '2026-11-11' && selDay.textContent.includes('4:15'));
check('week strip shows week total', window.document.querySelector('.ws-week').textContent.trim() === 'Week total 4:15');

window.document.getElementById('day-next').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(200);
check('day navigator moves forward', window.document.getElementById('day-date').value === '2026-11-12');

window.document.getElementById('copy-prev').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(200);
const copyRow = window.document.querySelector('#copy-rows .copy-row');
check('copy-from-previous-day lists the P-9 row', !!copyRow && copyRow.textContent.includes('P-9'));
copyRow.querySelector('input').value = '3';
copyRow.querySelector('button').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(250);
const day12 = await (await fetch(BASE + '/entries?date=2026-11-12')).json();
const copied = day12.entries.find((e) => e.project_code === 'P-9' && e.hours === 3);
check('copied row saved through the API', !!copied);
// Clean up so later week-view assertions stay at 4.25h.
await fetch(`${BASE}/entries/${copied.id}`, { method: 'DELETE', headers: { Cookie: SESSION_COOKIE } });

// ---- create a brand-new customer through the customers form ----
const custForm = window.document.getElementById('customer-form');
window.document.getElementById('customer-name').value = 'Globex';
window.document.getElementById('customer-currency').value = 'usd';
window.document.getElementById('customer-rate').value = '45.50';
custForm.dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);

const customers = (await (await fetch(BASE + '/customers')).json()).customers;
const globex = customers.find((c) => c.name === 'Globex');
check('customer form created Globex', !!globex);
check('currency normalised to USD', globex && globex.currency === 'USD');
check('rate stored as 4550 minor units', globex && globex.default_rate_minor === 4550);

// ---- reports tab runs a query and renders totals ----
const reportForm = window.document.getElementById('report-form');
window.document.getElementById('report-from').value = '2026-11-01';
window.document.getElementById('report-to').value = '2026-11-30';
window.document.getElementById('report-group').value = 'project';
reportForm.dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);
const reportRows = window.document.querySelectorAll('#report-table tbody tr');
check('report renders grouped rows', reportRows.length >= 1);
const csvHref = window.document.getElementById('csv-link').getAttribute('href');
check('CSV download link wired', csvHref && csvHref.includes('export.csv'));

// ---- tab keyboard nav moves selection ----
const tabDay = window.document.getElementById('tab-day');
tabDay.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
await tick(50);
check('arrow key switches tab', window.document.getElementById('tab-week').getAttribute('aria-selected') === 'true');

// ---- WEEK GRID VIEW ----
const tabWeek = window.document.getElementById('tab-week');
tabWeek.dispatchEvent(new window.Event('click', { bubbles: true }));
window.document.getElementById('week-date').value = '2026-11-11';
window.document.getElementById('week-picker').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);
const wkRows = window.document.querySelectorAll('#week-table tbody tr');
const wkRowText = wkRows[0] ? wkRows[0].textContent : '';
check('week grid renders the ACME / P-9 row', wkRowText.includes('ACME') && wkRowText.includes('P-9'));
const filled = [...window.document.querySelectorAll('#week-table td.cell.has')];
check('week grid marks the 2026-11-11 cell with hours', filled.some((c) => c.textContent.trim() === '4.25'));
check('week row total is 4.25', wkRowText.trim().endsWith('4.25'));

// Click the filled cell -> jumps to Day tab with the entry loaded for edit.
filled[0].querySelector('button').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(300);
check('cell click opens the entry for editing', window.document.getElementById('entry-id').value !== '');

// ---- PROJECT FORM: prefill from customer + required currency/rate (#11) ----
const tabCust = window.document.getElementById('tab-customers');
tabCust.dispatchEvent(new window.Event('click', { bubbles: true }));
// Click "Projects" on the ACME row to open the project form.
const projBtn = [...window.document.querySelectorAll('#customer-table button')].find((b) => b.textContent === 'Projects');
projBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(150);
check('project currency prefilled from customer', window.document.getElementById('project-currency').value === 'EUR');
check('project rate prefilled from customer default', window.document.getElementById('project-rate').value === '60.00');
// Create a project through the form with an override rate.
window.document.getElementById('project-code').value = 'mkt-2';
window.document.getElementById('project-rate').value = '95.00';
window.document.getElementById('project-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);
const projs = (await (await fetch(BASE + `/customers/${acmeOpt.value}/projects`)).json()).projects;
const mkt = projs.find((p) => p.code === 'MKT-2');
check('project form created MKT-2 with prefilled EUR + override rate', mkt && mkt.currency === 'EUR' && mkt.rate_minor === 9500);

// ---- TASK MANAGER (#38): add a task to MKT-2, then log time against it ----
const projRows = [...window.document.querySelectorAll('#project-table tbody tr')];
const mktRow = projRows.find((r) => r.cells[0].textContent === 'MKT-2');
const tasksBtn = [...mktRow.querySelectorAll('button')].find((b) => b.textContent === 'Tasks');
tasksBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(150);
window.document.getElementById('task-code').value = 't1';
window.document.getElementById('task-name').value = 'Sprint';
window.document.getElementById('task-rate').value = '95.00';
window.document.getElementById('task-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);
const tasks = (await (await fetch(BASE + `/customers/${acmeOpt.value}/projects/MKT-2/tasks`)).json()).tasks;
const t1 = tasks.find((t) => t.code === 'T1');
check('task created via form with rate override', t1 && t1.rate_minor === 9500);

// Entry form: choose MKT-2 -> task picker loads T1 -> log 1h against it.
const custSel2 = window.document.getElementById('entry-customer');
custSel2.value = acmeOpt.value;
custSel2.dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(150);
const projSel = window.document.getElementById('entry-project');
projSel.value = 'MKT-2';
projSel.dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(150);
const taskSel = window.document.getElementById('entry-task');
check('entry task picker loaded T1', [...taskSel.options].some((o) => o.value === 'T1'));
taskSel.value = 'T1';
window.document.getElementById('entry-date').value = '2026-12-01';
window.document.getElementById('entry-hours').value = '1';
window.document.getElementById('entry-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);
const dec = (await (await fetch(BASE + '/entries?date=2026-12-01')).json()).entries;
check('entry logged against task T1', dec.some((e) => e.task_code === 'T1'));

// ---- INVOICES (#8): generate a draft for ACME's billable work, then issue ----
const tabInv = window.document.getElementById('tab-invoices');
tabInv.dispatchEvent(new window.Event('click', { bubbles: true }));
window.document.getElementById('invoice-customer').value = acmeOpt.value;
window.document.getElementById('invoice-from').value = '2026-12-01';
window.document.getElementById('invoice-to').value = '2026-12-31';
window.document.getElementById('invoice-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(300);
const invs = (await (await fetch(BASE + '/invoices')).json()).invoices;
const acmeInv = invs.find((i) => i.customer_id === acmeOpt.value);
check('draft invoice generated', !!acmeInv && acmeInv.status === 'draft');
check('invoice total reflects billable work (MKT-2 1h x 95 = 95.00)', acmeInv && acmeInv.total_minor === 9500);
// Issue it via the API and confirm the invoiced entry is now locked.
const issueRes = await fetch(`${BASE}/invoices/${acmeInv.id}/issue`, { method: 'POST', headers: { Cookie: SESSION_COOKIE } });
check('invoice issues', issueRes.status === 200);
const invoicedEntryId = acmeInv.lines[0].entry_id;
const editRes = await fetch(`${BASE}/entries/${invoicedEntryId}`, {
  method: 'PUT',
  headers: { 'content-type': 'application/json', Cookie: SESSION_COOKIE },
  body: JSON.stringify({ date: '2026-12-01', customer_id: acmeOpt.value, project_code: 'MKT-2', hours: 1 }),
});
check('issuing locks the invoiced entry (edit -> 409)', editRes.status === 409);

// ---- EXPENSES (#23): add a category + an expense via the forms ----
const tabExp = window.document.getElementById('tab-expenses');
tabExp.dispatchEvent(new window.Event('click', { bubbles: true }));
window.document.getElementById('category-name').value = 'Travel';
window.document.getElementById('category-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(200);
const cats = (await (await fetch(BASE + '/categories')).json()).categories;
check('category created via form', cats.some((c) => c.name === 'Travel'));
const travel = cats.find((c) => c.name === 'Travel');
window.document.getElementById('expense-date').value = '2026-10-05';
window.document.getElementById('expense-customer').value = acmeOpt.value;
window.document.getElementById('expense-customer').dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(150);
window.document.getElementById('expense-category').value = travel.id;
window.document.getElementById('expense-amount').value = '125.00';
window.document.getElementById('expense-currency').value = 'EUR';
window.document.getElementById('expense-note').value = 'client visit flight';
window.document.getElementById('expense-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);
const exps = (await (await fetch(BASE + '/expenses')).json()).expenses;
const ex = exps.find((e) => e.note === 'client visit flight');
check('expense created via form with category + amount', ex && ex.amount_minor === 12500 && ex.category_id === travel.id);

// ---- SUBMISSIONS (#16): submit a fresh week via the GUI ----
const fresh = await (await fetch(BASE + '/entries', { method: 'POST', headers: { 'content-type': 'application/json', Cookie: SESSION_COOKIE }, body: JSON.stringify({ date: '2027-01-05', customer_id: acmeOpt.value, project_code: 'P-9', hours: 2 }) })).json();
const tabSub = window.document.getElementById('tab-submissions');
tabSub.dispatchEvent(new window.Event('click', { bubbles: true }));
window.document.getElementById('submission-week').value = '2027-01-04';
window.document.getElementById('submission-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(250);
const subs = (await (await fetch(BASE + '/submissions')).json()).submissions;
const jan = subs.find((x) => x.week_start === '2027-01-04');
check('submission created via GUI', jan && jan.state === 'submitted' && jan.entry_ids.includes(fresh.id));
const lockRes = await fetch(`${BASE}/entries/${fresh.id}`, { method: 'PUT', headers: { 'content-type': 'application/json', Cookie: SESSION_COOKIE }, body: JSON.stringify({ date: '2027-01-05', customer_id: acmeOpt.value, project_code: 'P-9', hours: 3 }) });
check('submitted week locks its entries (edit -> 409)', lockRes.status === 409);

console.log(`\n${failures === 0 ? 'ALL GUI CHECKS PASSED' : failures + ' GUI CHECK(S) FAILED'}`);
process.exit(failures === 0 ? 0 : 1);

// ---- WEEK GRID VIEW: render a project x day grid and click an empty cell ----
console.log('--- extra: week view ---');
