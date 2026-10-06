// Headless GUI smoke test: load the real index.html + app.js into jsdom,
// wire fetch to the live server, run boot(), then simulate adding an entry
// and creating a customer through the actual forms. Verifies the presentation
// layer drives the API and renders results.

import { JSDOM, VirtualConsole } from 'jsdom';
import fs from 'node:fs';

// CI serves on :8099; local runs can point elsewhere (TT_GUI_BASE) so a
// dev's harness server can never shadow the runner's own container.
const BASE = process.env.TT_GUI_BASE || 'http://localhost:8099';

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
// No window.confirm/prompt/alert stubs: the app drives one inline <dialog>
// (#102) and the checks below exercise it like a user would.
if (!window.Element.prototype.scrollIntoView) window.Element.prototype.scrollIntoView = () => {};
// jsdom has no blob URLs; the #113 download check stubs these and captures
// the transient <a download> instead of letting jsdom attempt a navigation.
window.URL.createObjectURL = () => 'blob:jsdom-stub';
window.URL.revokeObjectURL = () => {};

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
// NOTE: no API seeding of customers/projects here — the first-run setup
// wizard (#111) creates ACME + P-9 through the real UI below, exercising the
// trigger, the steps and the prefill rule end-to-end.

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

// ---- FIRST-RUN WIZARD (#111): auto-open on zero customers ----
const wzDlg = window.document.getElementById('wizard-dialog');
const wzOpen = () => wzDlg.open === true || wzDlg.hasAttribute('open');
check('wizard auto-opens as a modal on first login with no customers', wzOpen());
check('wizard starts on the welcome step', !window.document.getElementById('wz-step-0').hidden);
window.document.getElementById('wz-later').dispatchEvent(new window.Event('click', { bubbles: true }));
check('"Set up later" dismisses the wizard cleanly', !wzOpen());
// The sidebar icon sits directly under "Customers & projects" and reopens it.
const wzOrder = [...window.document.querySelectorAll('#tabs button')].map((b) => b.id);
check('wizard entry is directly under Customers & projects', wzOrder.indexOf('wizard-open') === wzOrder.indexOf('tab-customers') + 1);
window.document.getElementById('wizard-open').dispatchEvent(new window.Event('click', { bubbles: true }));
check('the icon reopens the wizard fresh', wzOpen() && !window.document.getElementById('wz-step-0').hidden);
window.document.getElementById('wz-next').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(80);
check('Next advances to the customer step', !window.document.getElementById('wz-step-1').hidden && window.document.getElementById('wz-step-0').hidden);
window.document.getElementById('wz-next').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(80);
check('empty customer name shows an inline error (no silent partial save)',
  !window.document.getElementById('wz-error').hidden);
window.document.getElementById('wz-cust-name').value = 'ACME';
window.document.getElementById('wz-cust-currency').value = 'eur';
window.document.getElementById('wz-cust-rate').value = '60.00';
window.document.getElementById('wz-next').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(400);
check('wizard advanced to the project step', !window.document.getElementById('wz-step-2').hidden);
check('project currency/rate prefilled from the customer default (#11)',
  window.document.getElementById('wz-project-currency').value === 'EUR'
  && window.document.getElementById('wz-project-rate').value === '60.00');
window.document.getElementById('wz-project-code').value = 'p-9';
window.document.getElementById('wz-project-name').value = 'Portal';
window.document.getElementById('wz-next').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(400);
const custs111 = (await (await fetch(BASE + '/customers', { headers: { Cookie: SESSION_COOKIE } })).json()).customers;
const acme111 = custs111.find((c) => c.name === 'ACME');
const projs111 = acme111
  ? (await (await fetch(`${BASE}/customers/${acme111.id}/projects`, { headers: { Cookie: SESSION_COOKIE } })).json()).projects
  : [];
check('wizard created customer + project through the real routes',
  !!acme111 && acme111.default_rate_minor === 6000
  && projs111.some((p) => p.code === 'P-9' && p.currency === 'EUR' && p.rate_minor === 6000));
check('done step names what was set up',
  !window.document.getElementById('wz-step-3').hidden
  && window.document.getElementById('wz-done-text').textContent.includes('ACME'));
// Finish: dialog closes, Day view with the pair preselected.
window.document.getElementById('wz-next').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(500);
check('wizard closed on finish', !wzOpen());
check('lands on the Timesheets (Day) view',
  window.document.getElementById('tab-timesheet').getAttribute('aria-selected') === 'true');
check('new customer + project preselected in the entry form',
  window.document.getElementById('entry-customer').value === acme111.id
  && window.document.getElementById('entry-project').value === 'P-9');
// Reopen with customers present jumps straight to the project step.
window.document.getElementById('wizard-open').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(120);
check('reopen with existing customers skips straight to the project step',
  wzOpen() && !window.document.getElementById('wz-step-2').hidden && window.document.getElementById('wz-step-0').hidden);
window.document.getElementById('wz-later').dispatchEvent(new window.Event('click', { bubbles: true }));
// Never auto-opens again once a customer exists: boot a second app instance
// against the same (now non-empty) server.
const dom2 = new JSDOM(html, { runScripts: 'outside-only', pretendToBeVisual: true, url: BASE + '/', virtualConsole: vc });
dom2.window.fetch = window.fetch;
if (!dom2.window.Element.prototype.scrollIntoView) dom2.window.Element.prototype.scrollIntoView = () => {};
dom2.window.URL.createObjectURL = () => 'blob:jsdom-stub';
dom2.window.URL.revokeObjectURL = () => {};
dom2.window.eval(appJs);
dom2.window.document.dispatchEvent(new window.Event('DOMContentLoaded', { bubbles: true }));
await tick(600);
check('wizard never auto-opens again once a customer exists',
  !(dom2.window.document.getElementById('wizard-dialog').open === true
    || dom2.window.document.getElementById('wizard-dialog').hasAttribute('open')));
dom2.window.close();

// ---- picker shows the ACME the wizard just created ----
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
check('day total reflects 4.25h as H:MM', totals.trim() === '4:15');

// ---- live region announced the success (accessibility) ----
const live = window.document.getElementById('live-region').textContent;
check('aria-live region announced an add', /added|updated/i.test(live));

// ---- inline <dialog> (#102): delete confirms through it, cancel keeps ----
const dlgNode = window.document.getElementById('app-dialog');
const isOpen = () => dlgNode.open === true || dlgNode.hasAttribute('open');
const dayRow0 = window.document.querySelector('#day-table tbody tr');
const dlgDel = [...dayRow0.querySelectorAll('button')].find((b) => b.textContent === 'Delete');
dlgDel.dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(120);
check('delete click opens the inline dialog', isOpen());
check(
  'dialog message names the entry being deleted',
  /Delete this entry \(4\.25h on P-9\)/.test(window.document.getElementById('dlg-message').textContent),
);
window.document.getElementById('dlg-cancel').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(120);
check('dialog closes on cancel', !isOpen());
const kept = await (await fetch(BASE + '/entries?date=2026-11-11')).json();
check('cancel keeps the entry (nothing deleted)', kept.entries.some((e) => e.id === made.id));
// Confirm path: same flow, OK accepts and the entry disappears.
dlgDel.dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(120);
window.document.getElementById('dlg-ok').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(400);
const gone = await (await fetch(BASE + '/entries?date=2026-11-11')).json();
check('confirming in the dialog deletes the entry', !gone.entries.some((e) => e.id === made.id));
// Restore the entry (later checks assert the 4.25h day) and re-render.
await postJson('/entries', {
  date: '2026-11-11',
  customer_id: acmeOpt.value,
  project_code: 'P-9',
  hours: 4.25,
  note: 'gui-test entry',
  billable: true,
});
window.document.getElementById('day-prev').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(150);
window.document.getElementById('day-next').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(250);

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

const copySel = window.document.getElementById('copy-days');
copySel.value = '1'; // one day back from 2026-11-12 -> the 2026-11-11 entry
copySel.dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(200);
const copyRow = window.document.querySelector('#copy-rows .copy-row');
check('copy-from-N-days lists the P-9 row', !!copyRow && copyRow.textContent.includes('P-9'));
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
// C7 regression: every customer picker refreshes immediately (used to need a
// page reload, so a new customer could not be invoiced/expensed/timed).
const optHas = (id, name) => [...window.document.getElementById(id).options].some((o) => o.textContent.includes(name));
check('invoice picker sees the new customer', optHas('invoice-customer', 'Globex'));
check('expense picker sees the new customer', optHas('expense-customer', 'Globex'));
check('timer picker sees the new customer', optHas('timer-customer', 'Globex'));
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
const tabDay = window.document.getElementById('ts-day');
tabDay.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
await tick(50);
check('arrow key switches tab', window.document.getElementById('ts-week').getAttribute('aria-selected') === 'true');

// ---- WEEK GRID VIEW (#13): inline editable cells ----
const tabWeek = window.document.getElementById('ts-week');
tabWeek.dispatchEvent(new window.Event('click', { bubbles: true }));
window.document.getElementById('week-date').value = '2026-11-11';
window.document.getElementById('week-date').dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(300);
const wkRows = window.document.querySelectorAll('#week-table tbody tr');
const wkRowText = wkRows[0] ? wkRows[0].textContent : '';
check('week grid renders the ACME / P-9 row', wkRowText.includes('ACME') && wkRowText.includes('P-9'));
const cellOf = (date) => window.document.querySelector(`#week-table input.cell-input[data-date="${date}"]`);
const filledCell = cellOf('2026-11-11');
check('filled cell is an editable input holding 4.25', filledCell && filledCell.value === '4.25');
check('week row total is H:MM', wkRows[0].querySelector('.row-total').textContent.trim() === '4:15');
const dayTotals = [...window.document.querySelectorAll('#week-table tfoot .day-total')].map((t) => t.textContent.trim());
check('day totals row renders H:MM per day', dayTotals.length === 7 && dayTotals[2] === '4:15' && dayTotals.filter((t) => t !== '4:15').every((t) => t === '0'));
check('week total footer is H:MM', window.document.querySelector('#week-table tfoot .week-total').textContent.trim() === '4:15');

// Inline save: type into the empty 2026-11-09 cell and blur -> POST.
const emptyCell = cellOf('2026-11-09');
emptyCell.value = '2.5';
emptyCell.dispatchEvent(new window.Event('focusout', { bubbles: true }));
await tick(400);
const sep9 = await (await fetch(BASE + '/entries?date=2026-11-09')).json();
check('inline cell save created an entry via POST', sep9.entries.some((e) => e.hours === 2.5 && e.project_code === 'P-9'));
check('row total updated to 6:45 after inline save', window.document.querySelector('#week-table tbody tr .row-total').textContent.trim() === '6:45');

// Clear-to-delete: blank the same cell and blur -> confirm + DELETE.
const savedCell = cellOf('2026-11-09');
check('cleared cell re-rendered with the saved value', savedCell && savedCell.value === '2.50');
savedCell.value = '';
savedCell.dispatchEvent(new window.Event('focusout', { bubbles: true }));
await tick(150);
check(
  'clearing a cell asks for confirmation in the dialog',
  window.document.getElementById('app-dialog').open === true
    || window.document.getElementById('app-dialog').hasAttribute('open'),
);
window.document.getElementById('dlg-ok').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(400);
const sep9b = await (await fetch(BASE + '/entries?date=2026-11-09')).json();
check('clearing the cell deleted the entry', sep9b.entries.every((e) => e.hours !== 2.5));

// Note indicator on the cell carrying 'gui-test entry' -> opens the Day editor.
const flag = window.document.querySelector('#week-table .note-flag');
check('note indicator shown for cells with notes', !!flag);
flag.dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(400);
check('note indicator opens the entry for editing', window.document.getElementById('entry-id').value !== '');
window.document.getElementById('ts-week').dispatchEvent(new window.Event('click', { bubbles: true }));

// Add row + copy-last-week controls exist.
check('add-row control present', !!window.document.getElementById('week-add-row'));
check('copy-from-week dropdown present', !!window.document.getElementById('week-copy-weeks'));

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

// ---- INVOICE PDF (#113): archived at issue, downloadable, hinted in JSON ----
const issuedBody = await issueRes.clone().json();
check('issue persists the pdf hint', issuedBody.pdf && /^INV-\d+\.pdf$/.test(issuedBody.pdf.filename) && issuedBody.pdf.sha256.length === 64 && issuedBody.pdf.bytes > 100);
const pdfRes = await fetch(`${BASE}/invoices/${acmeInv.id}/pdf`, { headers: { Cookie: SESSION_COOKIE } });
const pdfHead = new Uint8Array(await pdfRes.arrayBuffer().then((b) => b.slice(0, 8)));
check(
  'GET /invoices/{id}/pdf serves the archived PDF',
  pdfRes.status === 200
    && (pdfRes.headers.get('content-type') || '').includes('application/pdf')
    && String.fromCharCode(...pdfHead) === '%PDF-1.4',
);
check('PDF download names the file by invoice number', /filename="INV-\d+\.pdf"/.test(pdfRes.headers.get('content-disposition') || ''));

// ---- EMAIL (#35): set a billing email, then email the issued invoice ----
const custObj = (await (await fetch(BASE + `/customers/${acmeOpt.value}`, { headers: { Cookie: SESSION_COOKIE } })).json());
custObj.email = 'billing@acme.test';
const putC = await fetch(`${BASE}/customers/${acmeOpt.value}`, {
  method: 'PUT',
  headers: { 'content-type': 'application/json', Cookie: SESSION_COOKIE },
  body: JSON.stringify({ name: custObj.name, currency: custObj.currency, default_rate_minor: custObj.default_rate_minor, active: custObj.active, email: 'billing@acme.test' }),
});
check('customer email round-trips via API', putC.status === 200 && (await putC.json()).email === 'billing@acme.test');
const emailRes = await fetch(`${BASE}/invoices/${acmeInv.id}/email`, { method: 'POST', headers: { Cookie: SESSION_COOKIE } });
const emailBody = await emailRes.json();
check('invoice email endpoint sends to the billing address', emailRes.status === 200 && emailBody.sent_to === 'billing@acme.test');

// The GUI shows an Email button on the issued invoice and announces the send.
window.document.getElementById('tab-invoices').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(300); // tab is shown -> refreshInvoices() re-pulls, now reflecting the issued state
const emailBtn = [...window.document.querySelectorAll('#invoice-table tbody button')].find((b) => b.textContent === 'Email');
check('issued invoice renders an Email button', !!emailBtn);
if (emailBtn) {
  emailBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(300);
  {
  const t = window.document.getElementById('live-region').textContent;
  // #130 honesty: either a real send (smtp) or an explicit NOT-sent with the
  // address — never a bare success claim when the transport is disabled.
  check('Email button announces honestly',
    /billing@acme.test/.test(t) && /Invoice emailed to|NOT sent \(SMTP not configured\)/.test(t));
}
}

// ---- PDF DOWNLOAD (#113): GUI button fetches the blob and saves it ----
const pdfBtn = [...window.document.querySelectorAll('#invoice-table tbody button')].find((b) => b.textContent === 'PDF');
check('issued invoice renders a PDF download button', !!pdfBtn);
if (pdfBtn) {
  // Capture the transient <a download> instead of letting jsdom navigate:
  // the app's contract is "blob fetch (cookie intact) -> object URL -> a.click".
  let savedName = null;
  const origCreate = window.document.createElement.bind(window.document);
  window.document.createElement = (tag) => {
    const node = origCreate(tag);
    if (tag === 'a') node.addEventListener('click', (e) => { e.preventDefault(); savedName = node.download; });
    return node;
  };
  pdfBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(400);
  window.document.createElement = origCreate;
  check('PDF button saves the numbered file', !!savedName && /^INV-\d+\.pdf$/.test(savedName));
  check('PDF download announces success', /Invoice PDF downloaded/i.test(window.document.getElementById('live-region').textContent));

// ---- EMAIL COPY (#112): the GUI prompts for a recipient, then audited ----
const copyBtn = [...window.document.querySelectorAll('#invoice-table tbody button')].find((b) => b.textContent === 'Email copy');
check('issued invoice renders an Email copy button', !!copyBtn);
if (copyBtn) {
  copyBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(200);
  const dlg = window.document.getElementById('app-dialog');
  check('Email copy prompts for a recipient address', dlg.open === true || dlg.hasAttribute('open'));
  window.document.getElementById('dlg-input').value = 'accountant@gui.test';
  window.document.getElementById('dlg-ok').dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(400);
  check('Email copy announces honestly', /accountant@gui.test/.test(window.document.getElementById('live-region').textContent));
  const audit = await (await fetch(BASE + '/audit', { headers: { Cookie: SESSION_COOKIE } })).json();
  check(
    'Email copy lands in the audit log with the recipient',
    audit.events.some((e) => e.event === 'invoice_email_copy' && e.subject.includes('accountant@gui.test')),
  );
}
}

// ---- PAYMENTS (#34): checkout link via the Pay link button + webhook pays it ----
const payBtn = [...window.document.querySelectorAll('#invoice-table tbody button')].find((b) => b.textContent === 'Pay link');
check('issued invoice renders a Pay link button', !!payBtn);
if (payBtn) {
  payBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(300);
  const liveTxt = window.document.getElementById('live-region').textContent;
  check('Pay link announces a checkout URL', /checkout link: https:\/\//i.test(liveTxt));
  // The URL is offered for copying in the prompt dialog (#102), not window.prompt.
  const payDlgOpen = window.document.getElementById('app-dialog').open === true
    || window.document.getElementById('app-dialog').hasAttribute('open');
  check(
    'Pay link offers the URL in the input dialog',
    payDlgOpen && window.document.getElementById('dlg-input').value.startsWith('https://'),
  );
  window.document.getElementById('dlg-ok').dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(100);
  // A signed webhook (fake-mode Stripe, signature ignored) marks the invoice paid.
  const chk = await (await fetch(`${BASE}/invoices/${acmeInv.id}/checkout`, { method: 'POST', headers: { 'content-type': 'application/json', Cookie: SESSION_COOKIE }, body: JSON.stringify({ provider: 'stripe' }) })).json();
  const whBody = JSON.stringify({ type: 'checkout.session.completed', payment_status: 'paid', amount_minor: 9500, currency: 'EUR', client_reference_id: chk.reference, metadata: { invoice_number: acmeInv.number } });
  const whRes = await fetch(`${BASE}/payments/webhook/stripe`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: whBody });
  const whJson = await whRes.json();
  check('webhook marks the invoice paid', whRes.status === 200 && whJson.status === 'paid');
  const afterInv = (await (await fetch(BASE + '/invoices', { headers: { Cookie: SESSION_COOKIE } })).json()).invoices.find((i) => i.id === acmeInv.id);
  check('invoice now paid with a provider reference', afterInv.status === 'paid' && /stripe:/.test(afterInv.payment_reference));

  // ---- ACCOUNTING SYNC (#33): status endpoint + Sync button on the paid row ----
  const stRes = await fetch(BASE + '/sync/accounting', { headers: { Cookie: SESSION_COOKIE } });
  const stJson = await stRes.json();
  check('accounting status endpoint responds', stRes.status === 200 && Array.isArray(stJson.records));
  window.document.getElementById('tab-invoices').dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(300);
  const syncBtn = [...window.document.querySelectorAll('#invoice-table tbody button')].find((b) => b.textContent === 'Sync');
  check('paid invoice renders a Sync button', !!syncBtn);
  if (syncBtn) {
    syncBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
    await tick(300);
    check('Sync without a configured provider explains itself', /no accounting provider configured/i.test(window.document.getElementById('live-region').textContent));
  }

  // ---- SSO (#32): provider discovery endpoint (login screen advertises them) ----
  const ssoRes = await fetch(BASE + '/auth/sso/providers');
  const ssoJson = await ssoRes.json();
  check('SSO providers endpoint responds pre-session', ssoRes.status === 200 && Array.isArray(ssoJson.providers));
  check('login screen hides SSO box with none configured', window.document.getElementById('sso-box').hidden === true);
}

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

// ---- WEEK GRID (#13): add row, copy last week, lock column ----
window.document.getElementById('ts-week').dispatchEvent(new window.Event('click', { bubbles: true }));
const weekDateEl = window.document.getElementById('week-date');
weekDateEl.value = '2026-12-07';
weekDateEl.dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(300);

// Copy last week's project rows into an empty week (MKT-2 has 2026-12-01).
const copyWeeks = window.document.getElementById('week-copy-weeks');
copyWeeks.value = '1';
copyWeeks.dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(400);
const wkBody = window.document.querySelector('#week-table tbody');
const copiedRow = [...wkBody.querySelectorAll('tr')].find((r) => r.textContent.includes('MKT-2'));
check('copy-from-last-week added the MKT-2 row', !!copiedRow);
check('copied row starts empty', !!copiedRow && [...copiedRow.querySelectorAll('input.cell-input')].every((i) => i.value === ''));
check('copied row total is 0:00', copiedRow && copiedRow.querySelector('.row-total').textContent.trim() === '0:00');

// Add row control reveals the project picker and appends a new row.
window.document.getElementById('week-add-row').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(250);
const addSel = window.document.getElementById('week-add-project');
check('add-row picker lists active projects', !addSel.hidden && [...addSel.options].some((o) => o.textContent.includes('P-9')));
addSel.value = `${acmeOpt.value}|P-9`;
window.document.getElementById('week-add-confirm').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(400);
check('add-row appended the P-9 row', [...window.document.querySelectorAll('#week-table tbody tr')].some((r) => r.textContent.includes('P-9')));

// Lock column: the submitted 2027-01 week renders its cell disabled.
weekDateEl.value = '2027-01-05';
weekDateEl.dispatchEvent(new window.Event('change', { bubbles: true }));
await tick(300);
const janRow = [...window.document.querySelectorAll('#week-table tbody tr')].find(
  (r) => r.textContent.includes('P-9') && r.textContent.includes('ACME'),
);
const lockedCell = janRow && janRow.querySelector('input.cell-input[data-date="2027-01-05"]');
check('submitted-week cell holds the hours', !!lockedCell && lockedCell.value === '2.00');
check('submitted-week cell renders locked (disabled input)', !!lockedCell && lockedCell.disabled === true && lockedCell.closest('td').classList.contains('locked'));
check('locked row marked for the lock column', !!lockedCell && lockedCell.closest('tr').classList.contains('has-lock'));

// ---- CONFIG (#94): effective config renders + edit persists + source flips ----
const cfgRows = [...window.document.querySelectorAll('#config-table tbody tr')];
check('config table renders whitelisted rows', cfgRows.length >= 5);
const daysRow = cfgRows.find((r) => r.cells[0].textContent === 'reminder_days');
check('reminder_days row shows default source', !!daysRow && daysRow.cells[2].textContent === 'default');
if (daysRow) {
  window.document.getElementById('config-key').value = 'reminder_days';
  window.document.getElementById('config-value').value = '12';
  window.document.getElementById('config-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
  await tick(300);
  const after = [...window.document.querySelectorAll('#config-table tbody tr')].find((r) => r.cells[0].textContent === 'reminder_days');
  check('edit persists to config.json and source becomes file', after.cells[1].textContent === '12' && after.cells[2].textContent === 'file');
}
// Non-whitelisted keys cannot be written even via the API.
const badCfg = await fetch(BASE + '/admin/config', { method: 'PUT', headers: { 'content-type': 'application/json', Cookie: SESSION_COOKIE, 'X-CSRF-Protection': '1' }, body: '{"smtp.password":"nope"}' });
check('secret-shaped config keys are refused', badCfg.status === 422);

// ---- INVOICE TEMPLATE (#116): admin editor, cheat-sheet, rejection, preview ----
window.document.getElementById('tab-settings').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(300);
check('Settings discloses the email transport state (#130)',
  /Email delivery:/i.test(window.document.getElementById('email-status').textContent));
const varRows = [...window.document.querySelectorAll('#template-vars tbody tr')];
check('template editor renders the variable cheat-sheet from the server', varRows.length >= 12 && varRows.some((r) => r.textContent.includes('%invoice_issue_month%')));
// Unknown variable: inline error, no save.
window.document.getElementById('template-subject').value = 'Invoice %invoice_number%';
window.document.getElementById('template-body').value = 'Dear %customer_name%,\n\n- consulting work';
window.document.getElementById('template-footer').value = 'Wire to IBAN TT16';
window.document.getElementById('template-terms').value = 'net_30';
window.document.getElementById('template-terms').dispatchEvent(new window.Event('change', { bubbles: true }));
window.document.getElementById('template-body').value = 'bad %nope% here';
window.document.getElementById('template-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(300);
check('unknown %variable% shows an inline error', !window.document.getElementById('template-error').hidden
  && window.document.getElementById('template-error').textContent.includes('%nope%'));
// Fix it and save; terms select toggles the days field for custom.
window.document.getElementById('template-body').value = 'Dear %customer_name%,\n\n- consulting work';
window.document.getElementById('template-terms').value = 'custom';
window.document.getElementById('template-terms').dispatchEvent(new window.Event('change', { bubbles: true }));
check('custom terms reveal the days input', !window.document.getElementById('template-terms-days-field').hidden);
window.document.getElementById('template-terms-days').value = '21';
window.document.getElementById('template-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(300);
const tpl = await (await fetch(BASE + '/admin/invoice-template', { headers: { Cookie: SESSION_COOKIE } })).json();
check('template saved with custom terms', tpl.template.body.includes('%customer_name%') && tpl.template.payment_terms.kind === 'custom' && tpl.template.payment_terms.days === 21);
check('template save announced', /template saved/i.test(window.document.getElementById('live-region').textContent));

// ---- CUSTOMER INVOICE FIELDS (#116): terms, subject, notes via the form ----
window.document.getElementById('tab-customers').dispatchEvent(new window.Event('click', { bubbles: true }));
window.document.getElementById('customer-name').value = 'Termy LLC';
window.document.getElementById('customer-currency').value = 'EUR';
window.document.getElementById('customer-rate').value = '50';
window.document.getElementById('customer-terms').value = 'upon_receipt';
window.document.getElementById('customer-subject').value = 'INV %invoice_number% for %customer_name%';
window.document.getElementById('customer-notes').value = 'Please quote the invoice number.';
window.document.getElementById('customer-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
await tick(400);
const termCusts = (await (await fetch(BASE + '/customers', { headers: { Cookie: SESSION_COOKIE } })).json()).customers;
const termy = termCusts.find((c) => c.name === 'Termy LLC');
check('customer invoice fields persist via the form', !!termy && termy.payment_terms.kind === 'upon_receipt'
  && termy.invoice_subject.includes('%invoice_number%') && termy.invoice_notes.includes('quote the invoice'));

// Document preview card uses GET /invoices/{id}/document on the newest invoice.
window.document.getElementById('tab-settings').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(400);
const prev = window.document.getElementById('template-preview');
check('template preview card shows the rendered subject', !prev.hidden
  && window.document.getElementById('template-preview-subject').textContent.includes('Invoice '));
check('preview body is plain text (textContent-only rule)', window.document.getElementById('template-preview-body').children.length === 0
  && window.document.getElementById('template-preview-body').textContent.includes('consulting work'));

// ---- PARTIAL PAYMENTS & WRITE-OFF (#114): record payment + write off via GUI ----
const paid2028 = await (await fetch(BASE + '/entries', {
  method: 'POST',
  headers: { 'content-type': 'application/json', Cookie: SESSION_COOKIE, 'X-CSRF-Protection': '1' },
  body: JSON.stringify({ date: '2028-02-02', customer_id: acmeOpt.value, project_code: 'MKT-2', hours: 4 }),
})).json();
const inv114 = await (await fetch(BASE + '/invoices', {
  method: 'POST',
  headers: { 'content-type': 'application/json', Cookie: SESSION_COOKIE, 'X-CSRF-Protection': '1' },
  body: JSON.stringify({ customer_id: acmeOpt.value, from: '2028-02-01', to: '2028-02-29' }),
})).json();
await fetch(`${BASE}/invoices/${inv114.id}/issue`, { method: 'POST', headers: { Cookie: SESSION_COOKIE, 'X-CSRF-Protection': '1' } });
window.document.getElementById('tab-invoices').dispatchEvent(new window.Event('click', { bubbles: true }));
await tick(300);
const balCells = [...window.document.querySelectorAll('#invoice-table tbody tr')]
  .filter((r) => r.textContent.includes(inv114.number))
  .map((r) => r.children[4].textContent.trim());
check('open invoice shows the outstanding Balance', balCells.length === 1 && balCells[0].includes('380.00'), `cells=${balCells}`);
const recBtn = [...window.document.querySelectorAll('#invoice-table tbody button')].find((b) => b.textContent === 'Record payment'
  && b.closest('tr').textContent.includes(inv114.number));
check('issued invoice renders Record payment (replacing Mark paid)', !!recBtn);
if (recBtn) {
  recBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(200);
  const dlgIn = window.document.getElementById('dlg-input');
  check('record-payment dialog prefills the balance', /^\d+\.\d\d$/.test(dlgIn.value));
  const owed = dlgIn.value;
  dlgIn.value = '10.00';
  window.document.getElementById('dlg-ok').dispatchEvent(new window.Event('click', { bubbles: true }));
  await tick(200);
  window.document.getElementById('dlg-ok').dispatchEvent(new window.Event('click', { bubbles: true })); // reference step: empty ok
  await tick(400);
  const after = await (await fetch(`${BASE}/invoices/${inv114.id}`, { headers: { Cookie: SESSION_COOKIE } })).json();
  check('partial payment recorded as partly_paid with a ledger line', after.status === 'partly_paid' && after.payments.length === 1 && after.payments[0].amount_minor === 1000);
  check('record payment announces remaining balance', /still outstanding/i.test(window.document.getElementById('live-region').textContent));
  // Write off the rest.
  const woBtn = [...window.document.querySelectorAll('#invoice-table tbody button')].find((b) => b.textContent === 'Write off'
    && b.closest('tr').textContent.includes(inv114.number));
  check('partly_paid invoice offers Write off', !!woBtn);
  if (woBtn) {
    woBtn.dispatchEvent(new window.Event('click', { bubbles: true }));
    await tick(200);
    window.document.getElementById('dlg-input').value = 'goodwill waiver';
    window.document.getElementById('dlg-ok').dispatchEvent(new window.Event('click', { bubbles: true }));
    await tick(200);
    window.document.getElementById('dlg-ok').dispatchEvent(new window.Event('click', { bubbles: true })); // confirm step
    await tick(400);
    const wo = await (await fetch(`${BASE}/invoices/${inv114.id}`, { headers: { Cookie: SESSION_COOKIE } })).json();
    check('write-off finalizes with the reason', wo.status === 'written_off' && wo.write_off_reason === 'goodwill waiver');
    check('write-off announced', /written off/i.test(window.document.getElementById('live-region').textContent));
  }
}

console.log(`\n${failures === 0 ? 'ALL GUI CHECKS PASSED' : failures + ' GUI CHECK(S) FAILED'}`);
process.exit(failures === 0 ? 0 : 1);

// ---- WEEK GRID VIEW: render a project x day grid and click an empty cell ----
console.log('--- extra: week view ---');
