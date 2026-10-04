// TucanoTime GUI. Pure presentation layer: every value comes from and goes to
// the JSON API. The server is the only actor that touches storage. All user
// data is inserted with textContent (never innerHTML) so stored notes/names
// cannot execute as markup.

'use strict';

const api = {
  async request(method, url, body) {
    const opts = { method, headers: {} };
    // CSRF guard (#46): required on all mutating requests.
    opts.headers['X-CSRF-Protection'] = '1';
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    const res = await fetch(url, opts);
    if (res.status === 204) return null;
    const text = await res.text();
    const data = text ? JSON.parse(text) : null;
    if (!res.ok) {
      const err = new Error(data?.error?.message || `HTTP ${res.status}`);
      err.status = res.status;
      err.payload = data;
      throw err;
    }
    return data;
  },
  get: (u) => api.request('GET', u),
  post: (u, b) => api.request('POST', u, b),
  put: (u, b) => api.request('PUT', u, b),
  del: (u) => api.request('DELETE', u),
};

const $ = (id) => document.getElementById(id);

function announce(msg) {
  $('live-region').textContent = msg;
}

function showFormError(el, err) {
  const fields = err.payload?.error?.fields;
  let text = err.message || 'The request failed.';
  if (Array.isArray(fields) && fields.length) {
    text = fields.map((f) => `${f.field}: ${f.message}`).join(' · ');
  }
  el.textContent = text;
  el.hidden = false;
}

function clearFormError(el) {
  el.textContent = '';
  el.hidden = true;
}

function isoDate(d) {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `${y}-${m}-${day}`;
}

function addDays(dateStr, n) {
  const [y, m, d] = dateStr.split('-').map(Number);
  const dt = new Date(Date.UTC(y, m - 1, d));
  dt.setUTCDate(dt.getUTCDate() + n);
  return isoDate(dt);
}

function mondayOf(dateStr) {
  const [y, m, d] = dateStr.split('-').map(Number);
  const dt = new Date(Date.UTC(y, m - 1, d));
  const dow = dt.getUTCDay(); // 0 Sun .. 6 Sat
  const shift = (dow === 0 ? -6 : 1) - dow;
  return addDays(dateStr, shift);
}

function formatMoney(minor) {
  return (minor / 100).toFixed(2);
}

// ----------------------------------------------------------------- cache ---

const state = {
  customers: [],           // Customer[]
  projectsByCustomer: {},  // id -> Project[]
};

async function loadCustomers() {
  const data = await api.get('/customers');
  state.customers = data.customers || [];
}

async function loadProjects(customerId) {
  const data = await api.get(`/customers/${customerId}/projects`);
  state.projectsByCustomer[customerId] = data.projects || [];
  return state.projectsByCustomer[customerId];
}

function customerName(id) {
  const c = state.customers.find((x) => x.id === id);
  return c ? c.name : '(deleted customer)';
}

// ------------------------------------------------------------- customer ---

function fillCustomerSelect(select, selectedId, includePlaceholder) {
  select.textContent = '';
  if (includePlaceholder) {
    const ph = document.createElement('option');
    ph.value = '';
    ph.textContent = 'Choose a customer…';
    select.appendChild(ph);
  }
  for (const c of state.customers) {
    const opt = document.createElement('option');
    opt.value = c.id;
    opt.textContent = c.active ? c.name : `${c.name} (inactive)`;
    if (c.id === selectedId) opt.selected = true;
    select.appendChild(opt);
  }
}

async function fillProjectSelect(select, customerId, selectedCode) {
  select.textContent = '';
  if (!customerId) {
    const ph = document.createElement('option');
    ph.value = '';
    ph.textContent = 'Choose a customer first…';
    select.appendChild(ph);
    return;
  }
  const projects = await loadProjects(customerId);
  const ph = document.createElement('option');
  ph.value = '';
  ph.textContent = 'Choose a project…';
  select.appendChild(ph);
  for (const p of projects) {
    const opt = document.createElement('option');
    opt.value = p.code;
    opt.textContent = p.active ? `${p.code} — ${p.name}` : `${p.code} — ${p.name} (inactive)`;
    if (p.code === selectedCode) opt.selected = true;
    select.appendChild(opt);
  }
}

// Loads a project's tasks into the given select, with a leading "None".
async function fillTaskSelect(select, customerId, projectCode, selectedCode) {
  select.textContent = '';
  const ph = document.createElement('option');
  ph.value = '';
  ph.textContent = projectCode ? 'None' : 'Choose a project first…';
  select.appendChild(ph);
  if (!customerId || !projectCode) return;
  const data = await api.get(
    `/customers/${customerId}/projects/${encodeURIComponent(projectCode)}/tasks`,
  );
  for (const t of data.tasks || []) {
    const opt = document.createElement('option');
    opt.value = t.code;
    opt.textContent = t.active ? `${t.code} — ${t.name}` : `${t.code} — ${t.name} (inactive)`;
    if (t.code === selectedCode) opt.selected = true;
    select.appendChild(opt);
  }
}

// ---------------------------------------------------------------- tabs ---

function initTabs() {
  const tabs = Array.from(document.querySelectorAll('[role="tab"]'));
  function activate(tab) {
    tabs.forEach((t) => {
      const on = t === tab;
      t.setAttribute('aria-selected', String(on));
      t.tabIndex = on ? 0 : -1;
      $(t.getAttribute('aria-controls')).hidden = !on;
    });
    const title = $('page-title');
    if (title) title.textContent = tab.textContent.trim();
    tab.focus();
  }
  tabs.forEach((t, i) => {
    t.addEventListener('click', () => activate(t));
    t.addEventListener('keydown', (e) => {
      let j = null;
      if (e.key === 'ArrowRight') j = (i + 1) % tabs.length;
      if (e.key === 'ArrowLeft') j = (i - 1 + tabs.length) % tabs.length;
      if (e.key === 'Home') j = 0;
      if (e.key === 'End') j = tabs.length - 1;
      if (j !== null) { e.preventDefault(); activate(tabs[j]); }
    });
  });
}

// ----------------------------------------------------------------- day ---

async function refreshDay() {
  const date = $('day-date').value;
  if (!date) return;
  $('day-label').textContent = date;
  const data = await api.get(`/entries?date=${encodeURIComponent(date)}`);
  const rows = data.entries || [];
  const tbody = $('day-table').querySelector('tbody');
  tbody.textContent = '';
  let total = 0;
  for (const e of rows) {
    total += Math.round(e.hours * 100);
    const tr = document.createElement('tr');

    const tdCust = document.createElement('td');
    tdCust.textContent = customerName(e.customer_id);
    const tdProj = document.createElement('td');
    tdProj.textContent = e.project_code;
    const tdHrs = document.createElement('td');
    tdHrs.className = 'num';
    tdHrs.textContent = e.hours.toFixed(2);
    const tdNote = document.createElement('td');
    tdNote.textContent = e.note || '';
    if (!e.billable) {
      const nb = document.createElement('span');
      nb.className = 'badge';
      nb.textContent = ' non-billable';
      tdNote.appendChild(nb);
    }
    const tdAct = document.createElement('td');
    tdAct.className = 'actions-col';

    const edit = document.createElement('button');
    edit.className = 'link';
    edit.type = 'button';
    edit.textContent = 'Edit';
    edit.addEventListener('click', () => startEdit(e));
    const del = document.createElement('button');
    del.className = 'danger';
    del.type = 'button';
    del.textContent = 'Delete';
    del.addEventListener('click', () => removeEntry(e));

    tdAct.append(edit, del);
    tr.append(tdCust, tdProj, tdHrs, tdNote, tdAct);
    tbody.appendChild(tr);
  }
  $('day-total').textContent = (total / 100).toFixed(2);
  $('day-table').hidden = rows.length === 0;
  $('day-empty').hidden = rows.length !== 0;
}

function startEdit(e) {
  $('entry-id').value = e.id;
  $('entry-date').value = e.date;
  $('entry-customer').value = e.customer_id;
  fillProjectSelect($('entry-project'), e.customer_id, e.project_code);
  fillTaskSelect($('entry-task'), e.customer_id, e.project_code, e.task_code);
  $('entry-hours').value = e.hours;
  $('entry-note').value = e.note || '';
  $('entry-billable').checked = e.billable !== false;
  $('entry-form-title').textContent = `Edit entry ${e.id.slice(0, 8)}`;
  $('entry-save').textContent = 'Update entry';
  $('entry-cancel').hidden = false;
  clearFormError($('entry-error'));
  $('entry-customer').focus();
  $('entry-form').scrollIntoView({ behavior: 'smooth', block: 'center' });
}

function resetEntryForm() {
  $('entry-id').value = '';
  $('entry-hours').value = '';
  $('entry-note').value = '';
  $('entry-billable').checked = true;
  $('entry-date').value = $('day-date').value || isoDate(new Date());
  $('entry-project').value = '';
  fillTaskSelect($('entry-task'), '', null, null);
  $('entry-form-title').textContent = 'Add an entry';
  $('entry-save').textContent = 'Save entry';
  $('entry-cancel').hidden = true;
  clearFormError($('entry-error'));
}

async function removeEntry(e) {
  const label = `${e.hours}h on ${e.project_code}`;
  if (!window.confirm(`Delete this entry (${label})? This cannot be undone.`)) return;
  try {
    await api.del(`/entries/${e.id}`);
    announce(`Deleted ${label}.`);
    await refreshDay();
  } catch (err) {
    announce(`Delete failed: ${err.message}`);
  }
}

async function saveEntry(evt) {
  evt.preventDefault();
  clearFormError($('entry-error'));
  const id = $('entry-id').value;
  const projectId = $('entry-project').value;
  if (!projectId) {
    showFormError($('entry-error'), { message: 'Select a project code.' });
    return;
  }
  const body = {
    date: $('entry-date').value,
    customer_id: $('entry-customer').value,
    project_code: projectId,
    task_code: $('entry-task').value || null,
    hours: Number($('entry-hours').value),
    note: $('entry-note').value,
    billable: $('entry-billable').checked,
  };
  try {
    if (id) {
      await api.put(`/entries/${id}`, body);
      announce('Entry updated.');
    } else {
      await api.post('/entries', body);
      announce('Entry added.');
    }
    // Keep the table aligned with whatever day was just written, even if it
    // differs from the one being viewed.
    $('day-date').value = body.date;
    const savedId = id || null;
    resetEntryForm();
    await refreshDay();
    if (!savedId) $('entry-hours').focus();
  } catch (err) {
    showFormError($('entry-error'), err);
  }
}

// ---------------------------------------------------------------- week ---

async function refreshWeek() {
  const anchor = $('week-date').value;
  if (!anchor) return;
  const start = mondayOf(anchor);
  const days = [];
  for (let i = 0; i < 7; i += 1) days.push(addDays(start, i));
  const end = days[6];
  $('week-label').textContent = `${start} → ${end}`;

  const data = await api.get(`/entries?from=${start}&to=${end}`);
  const entries = data.entries || [];

  const keyFor = (e) => `${e.customer_id}::${e.project_code}`;
  const byKey = new Map();
  for (const e of entries) {
    const k = keyFor(e);
    if (!byKey.has(k)) {
      byKey.set(k, { customer_id: e.customer_id, project_code: e.project_code, cells: {} });
    }
    const row = byKey.get(k);
    row.cells[e.date] = (row.cells[e.date] || 0) + Math.round(e.hours * 100);
    if (!row.ids) row.ids = {};
    row.ids[e.date] = e.id;
  }

  const table = $('week-table');
  table.textContent = '';

  const thead = document.createElement('thead');
  const htr = document.createElement('tr');
  const corner = document.createElement('th');
  corner.scope = 'col';
  corner.textContent = 'Project';
  htr.appendChild(corner);
  for (const d of days) {
    const th = document.createElement('th');
    th.scope = 'col';
    th.className = 'num';
    th.textContent = d.slice(5);
    htr.appendChild(th);
  }
  const totHead = document.createElement('th');
  totHead.scope = 'col';
  totHead.className = 'num';
  totHead.textContent = 'Week';
  htr.appendChild(totHead);
  thead.appendChild(htr);
  table.appendChild(thead);

  const tbody = document.createElement('tbody');
  const sorted = Array.from(byKey.values()).sort(
    (a, b) =>
      customerName(a.customer_id).localeCompare(customerName(b.customer_id)) ||
      a.project_code.localeCompare(b.project_code),
  );
  for (const row of sorted) {
    const tr = document.createElement('tr');
    const label = document.createElement('th');
    label.scope = 'row';
    label.textContent = `${customerName(row.customer_id)} / ${row.project_code}`;
    tr.appendChild(label);
    let weekTotal = 0;
    for (const d of days) {
      const td = document.createElement('td');
      td.className = 'num cell';
      const hundredths = row.cells[d];
      if (hundredths) {
        weekTotal += hundredths;
        td.classList.add('has');
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.textContent = (hundredths / 100).toFixed(2);
        btn.setAttribute(
          'aria-label',
          `${row.project_code} on ${d}: ${(hundredths / 100).toFixed(2)} hours. Adjust.`,
        );
        btn.addEventListener('click', () => jumpToEntry(row.ids[d]));
        td.appendChild(btn);
      } else {
        td.textContent = '';
        td.dataset.add = `${row.customer_id}|${row.project_code}|${d}`;
        td.addEventListener('click', () => prefillFromCell(row.customer_id, row.project_code, d));
      }
      tr.appendChild(td);
    }
    const tw = document.createElement('td');
    tw.className = 'num';
    tw.textContent = (weekTotal / 100).toFixed(2);
    tr.appendChild(tw);
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);

  $('week-table').hidden = sorted.length === 0;
  $('week-empty').hidden = sorted.length !== 0;
}

async function jumpToEntry(entryId) {
  const e = await api.get(`/entries/${entryId}`);
  switchTab('tab-day');
  $('day-date').value = e.date;
  await refreshDay();
  const target = $('day-table').querySelector('tbody tr');
  startEdit(e);
  if (target) target.scrollIntoView({ behavior: 'smooth' });
}

async function prefillFromCell(customerId, projectCode, date) {
  switchTab('tab-day');
  $('day-date').value = date;
  await refreshDay();
  $('entry-date').value = date;
  $('entry-customer').value = customerId;
  await fillProjectSelect($('entry-project'), customerId, projectCode);
  $('entry-hours').focus();
}

// ---------------------------------------------------------- customers ---

async function refreshCustomerTable() {
  const tbody = $('customer-table').querySelector('tbody');
  tbody.textContent = '';
  for (const c of state.customers) {
    const tr = document.createElement('tr');
    const name = document.createElement('th');
    name.scope = 'row';
    name.textContent = c.name;
    const cur = document.createElement('td');
    cur.textContent = c.currency;
    const rate = document.createElement('td');
    rate.className = 'num';
    rate.textContent = `${c.currency} ${formatMoney(c.default_rate_minor)}`;
    const status = document.createElement('td');
    const badge = document.createElement('span');
    badge.className = c.active ? 'badge on' : 'badge';
    badge.textContent = c.active ? 'Active' : 'Inactive';
    status.appendChild(badge);

    const act = document.createElement('td');
    act.className = 'actions-col';
    const projects = document.createElement('button');
    projects.className = 'link';
    projects.type = 'button';
    projects.textContent = 'Projects';
    projects.addEventListener('click', () => selectCustomerForProjects(c.id));
    const edit = document.createElement('button');
    edit.className = 'link';
    edit.type = 'button';
    edit.textContent = 'Edit';
    edit.addEventListener('click', () => startCustomerEdit(c));
    const del = document.createElement('button');
    del.className = 'danger';
    del.type = 'button';
    del.textContent = 'Delete';
    del.addEventListener('click', () => removeCustomer(c));
    act.append(projects, edit, del);

    tr.append(name, cur, rate, status, act);
    tbody.appendChild(tr);
  }
}

function startCustomerEdit(c) {
  $('customer-id').value = c.id;
  $('customer-name').value = c.name;
  $('customer-currency').value = c.currency;
  $('customer-rate').value = (c.default_rate_minor / 100).toFixed(2);
  $('customer-active').checked = c.active;
  $('customer-save').textContent = 'Update customer';
  $('customer-cancel').hidden = false;
  clearFormError($('customer-error'));
  $('customer-name').focus();
}

async function saveCustomer(evt) {
  evt.preventDefault();
  clearFormError($('customer-error'));
  const id = $('customer-id').value;
  const body = {
    name: $('customer-name').value.trim(),
    currency: $('customer-currency').value.trim().toUpperCase(),
    default_rate_minor: Math.round(Number($('customer-rate').value) * 100),
    active: $('customer-active').checked,
  };
  try {
    if (id) await api.put(`/customers/${id}`, body);
    else await api.post('/customers', body);
    await loadCustomers();
    await refreshCustomerTable();
    await refreshCustomerPickers();
    announce(id ? 'Customer updated.' : 'Customer added.');
    cancelCustomerEdit();
  } catch (err) {
    showFormError($('customer-error'), err);
  }
}

function cancelCustomerEdit() {
  $('customer-id').value = '';
  $('customer-form').reset();
  $('customer-active').checked = true;
  $('customer-save').textContent = 'Add customer';
  $('customer-cancel').hidden = true;
  clearFormError($('customer-error'));
}

async function removeCustomer(c) {
  if (!window.confirm(`Delete customer "${c.name}"?`)) return;
  try {
    await api.del(`/customers/${c.id}`);
    await loadCustomers();
    await refreshCustomerTable();
    await refreshCustomerPickers();
    announce('Customer deleted.');
  } catch (err) {
    announce(err.message);
    window.alert(err.message);
  }
}

async function selectCustomerForProjects(cid) {
  state.selectedCustomerId = cid;
  $('project-customer-label').textContent = customerName(cid);
  $('project-form').hidden = false;
  $('project-table').hidden = false;
  $('project-form').reset();
  $('project-active').checked = true;
  $('project-original-code').value = '';
  $('project-save').textContent = 'Add project';
  $('project-cancel').hidden = true;
  // Prefill the required currency + rate from the customer default (#11).
  const cust = state.customers.find((c) => c.id === cid);
  if (cust) {
    $('project-currency').value = cust.currency;
    $('project-rate').value = (cust.default_rate_minor / 100).toFixed(2);
  }
  await refreshProjectTable();
}

async function refreshProjectTable() {
  const cid = state.selectedCustomerId;
  if (!cid) return;
  const projects = await loadProjects(cid);
  const tbody = $('project-table').querySelector('tbody');
  tbody.textContent = '';
  for (const p of projects) {
    const tr = document.createElement('tr');
    const code = document.createElement('th');
    code.scope = 'row';
    code.textContent = p.code;
    const name = document.createElement('td');
    name.textContent = p.name;
    const cur = document.createElement('td');
    cur.textContent = p.currency || 'customer';
    const rate = document.createElement('td');
    rate.className = 'num';
    rate.textContent = p.rate_minor != null ? formatMoney(p.rate_minor) : 'customer';
    const status = document.createElement('td');
    const badge = document.createElement('span');
    badge.className = p.active ? 'badge on' : 'badge';
    badge.textContent = p.active ? 'Active' : 'Inactive';
    status.appendChild(badge);
    const act = document.createElement('td');
    act.className = 'actions-col';
    const tasks = document.createElement('button');
    tasks.className = 'link';
    tasks.type = 'button';
    tasks.textContent = 'Tasks';
    tasks.addEventListener('click', () => selectProjectForTasks(p.code));
    const edit = document.createElement('button');
    edit.className = 'link';
    edit.type = 'button';
    edit.textContent = 'Edit';
    edit.addEventListener('click', () => startProjectEdit(p));
    const del = document.createElement('button');
    del.className = 'danger';
    del.type = 'button';
    del.textContent = 'Delete';
    del.addEventListener('click', () => removeProject(p));
    act.append(tasks, edit, del);
    tr.append(code, name, cur, rate, status, act);
    tbody.appendChild(tr);
  }
}

function startProjectEdit(p) {
  $('project-original-code').value = p.code;
  $('project-code').value = p.code;
  $('project-name').value = p.name;
  $('project-currency').value = p.currency || '';
  $('project-rate').value = p.rate_minor != null ? formatMoney(p.rate_minor) : '';
  $('project-active').checked = p.active;
  $('project-save').textContent = 'Update project';
  $('project-cancel').hidden = false;
  clearFormError($('project-error'));
  $('project-code').focus();
}

async function saveProject(evt) {
  evt.preventDefault();
  clearFormError($('project-error'));
  const cid = state.selectedCustomerId;
  const original = $('project-original-code').value;
  const code = $('project-code').value.trim().toUpperCase();
  const currency = $('project-currency').value.trim().toUpperCase();
  const rate = $('project-rate').value.trim();
  if (!currency || !rate) {
    showFormError($('project-error'), {
      message: 'Currency and hourly rate are required for a project.',
    });
    return;
  }
  const body = {
    code,
    name: $('project-name').value.trim(),
    currency,
    rate_minor: Math.round(Number(rate) * 100),
    active: $('project-active').checked,
  };
  try {
    if (original) await api.put(`/customers/${cid}/projects/${encodeURIComponent(original)}`, body);
    else await api.post(`/customers/${cid}/projects`, body);
    await refreshProjectTable();
    announce(original ? 'Project updated.' : 'Project added.');
    cancelProjectEdit();
  } catch (err) {
    showFormError($('project-error'), err);
  }
}

function cancelProjectEdit() {
  $('project-original-code').value = '';
  $('project-form').reset();
  $('project-active').checked = true;
  $('project-save').textContent = 'Add project';
  $('project-cancel').hidden = true;
  clearFormError($('project-error'));
}

async function removeProject(p) {
  if (!window.confirm(`Delete project ${p.code}?`)) return;
  const cid = state.selectedCustomerId;
  try {
    await api.del(`/customers/${cid}/projects/${encodeURIComponent(p.code)}`);
    await refreshProjectTable();
    announce('Project deleted.');
  } catch (err) {
    announce(err.message);
    window.alert(err.message);
  }
}

// ---------------------------------------------------------------- tasks ---

async function selectProjectForTasks(pcode) {
  state.selectedProjectCode = pcode;
  const cid = state.selectedCustomerId;
  const cust = state.customers.find((c) => c.id === cid);
  $('task-project-label').textContent = `${cust ? cust.name : ''} / ${pcode}`;
  $('task-form').hidden = false;
  $('task-table').hidden = false;
  cancelTaskEdit();
  await refreshTaskTable();
}

async function refreshTaskTable() {
  const cid = state.selectedCustomerId;
  const pcode = state.selectedProjectCode;
  if (!cid || !pcode) return;
  const data = await api.get(
    `/customers/${cid}/projects/${encodeURIComponent(pcode)}/tasks`,
  );
  const tbody = $('task-table').querySelector('tbody');
  tbody.textContent = '';
  for (const t of data.tasks || []) {
    const tr = document.createElement('tr');
    const code = document.createElement('th');
    code.scope = 'row';
    code.textContent = t.code;
    const name = document.createElement('td');
    name.textContent = t.name;
    const cur = document.createElement('td');
    cur.textContent = t.currency || 'project';
    const rate = document.createElement('td');
    rate.className = 'num';
    rate.textContent = t.rate_minor != null ? formatMoney(t.rate_minor) : 'project';
    const status = document.createElement('td');
    const badge = document.createElement('span');
    badge.className = t.active ? 'badge on' : 'badge';
    badge.textContent = t.active ? 'Active' : 'Inactive';
    status.appendChild(badge);
    const act = document.createElement('td');
    act.className = 'actions-col';
    const edit = document.createElement('button');
    edit.className = 'link';
    edit.type = 'button';
    edit.textContent = 'Edit';
    edit.addEventListener('click', () => startTaskEdit(t));
    const del = document.createElement('button');
    del.className = 'danger';
    del.type = 'button';
    del.textContent = 'Delete';
    del.addEventListener('click', () => removeTask(t));
    act.append(edit, del);
    tr.append(code, name, cur, rate, status, act);
    tbody.appendChild(tr);
  }
}

function startTaskEdit(t) {
  $('task-original-code').value = t.code;
  $('task-code').value = t.code;
  $('task-name').value = t.name;
  $('task-currency').value = t.currency || '';
  $('task-rate').value = t.rate_minor != null ? formatMoney(t.rate_minor) : '';
  $('task-active').checked = t.active;
  $('task-save').textContent = 'Update task';
  $('task-cancel').hidden = false;
  clearFormError($('task-error'));
  $('task-code').focus();
}

function cancelTaskEdit() {
  $('task-original-code').value = '';
  $('task-form').reset();
  $('task-active').checked = true;
  $('task-save').textContent = 'Add task';
  $('task-cancel').hidden = true;
  clearFormError($('task-error'));
}

async function saveTask(evt) {
  evt.preventDefault();
  clearFormError($('task-error'));
  const cid = state.selectedCustomerId;
  const pcode = state.selectedProjectCode;
  const original = $('task-original-code').value;
  const code = $('task-code').value.trim().toUpperCase();
  const body = { code, name: $('task-name').value.trim(), active: $('task-active').checked };
  const currency = $('task-currency').value.trim().toUpperCase();
  if (currency) body.currency = currency;
  const rate = $('task-rate').value.trim();
  if (rate) body.rate_minor = Math.round(Number(rate) * 100);
  const base = `/customers/${cid}/projects/${encodeURIComponent(pcode)}/tasks`;
  try {
    if (original) await api.put(`${base}/${encodeURIComponent(original)}`, body);
    else await api.post(base, body);
    await refreshTaskTable();
    announce(original ? 'Task updated.' : 'Task added.');
    cancelTaskEdit();
  } catch (err) {
    showFormError($('task-error'), err);
  }
}

async function removeTask(t) {
  if (!window.confirm(`Delete task ${t.code}?`)) return;
  const cid = state.selectedCustomerId;
  const pcode = state.selectedProjectCode;
  try {
    await api.del(
      `/customers/${cid}/projects/${encodeURIComponent(pcode)}/tasks/${encodeURIComponent(t.code)}`,
    );
    await refreshTaskTable();
    announce('Task deleted.');
  } catch (err) {
    announce(err.message);
    window.alert(err.message);
  }
}

async function refreshCustomerPickers() {
  // The day entry form picker plus the project picker for the chosen customer.
  const keep = $('entry-customer').value;
  fillCustomerSelect($('entry-customer'), keep, true);
  if (keep && !state.customers.some((c) => c.id === keep)) {
    await fillProjectSelect($('entry-project'), '', null);
  }
}

// ------------------------------------------------------------- reports ---

async function runReport(evt) {
  evt.preventDefault();
  const from = $('report-from').value;
  const to = $('report-to').value;
  const group = $('report-group').value;
  const billable = $('report-billable').value;
  let url = `/reports/summary?from=${from}&to=${to}&group=${group}`;
  if (billable) url += `&billable=${billable}`;
  const summary = await api.get(url);
  const tbody = $('report-table').querySelector('tbody');
  tbody.textContent = '';
  for (const row of summary.rows) {
    const tr = document.createElement('tr');
    const label = document.createElement('th');
    label.scope = 'row';
    label.textContent = row.label;
    const cur = document.createElement('td');
    cur.textContent = row.currency;
    const hrs = document.createElement('td');
    hrs.className = 'num';
    hrs.textContent = row.hours.toFixed(2);
    const amt = document.createElement('td');
    amt.className = 'num';
    amt.textContent = `${row.currency} ${formatMoney(row.amount_minor)}`;
    const ent = document.createElement('td');
    ent.className = 'num';
    ent.textContent = String(row.entries);
    tr.append(label, cur, hrs, amt, ent);
    tbody.appendChild(tr);
  }
  $('report-total').textContent = `${summary.total_hours.toFixed(2)}  (billable ${summary.billable_hours.toFixed(2)} / non-billable ${summary.nonbillable_hours.toFixed(2)})`;
  const link = $('csv-link');
  link.href = `/reports/export.csv?from=${from}&to=${to}`;
  link.hidden = false;
  announce(`Report ready: ${summary.rows.length} rows.`);
}

// -------------------------------------------------------------- invoices ---

async function refreshInvoices() {
  const data = await api.get('/invoices');
  const invoices = data.invoices || [];
  const tbody = $('invoice-table').querySelector('tbody');
  tbody.textContent = '';
  for (const inv of invoices) {
    const tr = document.createElement('tr');
    const num = document.createElement('th');
    num.scope = 'row';
    num.textContent = inv.number;
    const cust = document.createElement('td');
    cust.textContent = customerName(inv.customer_id);
    const period = document.createElement('td');
    period.textContent = `${inv.period_from} → ${inv.period_to}`;
    const total = document.createElement('td');
    total.className = 'num';
    total.textContent = `${inv.currency} ${formatMoney(inv.total_minor)}`;
    const status = document.createElement('td');
    const badge = document.createElement('span');
    badge.className = inv.status === 'issued' ? 'badge on' : 'badge';
    badge.textContent = inv.status;
    status.appendChild(badge);
    const act = document.createElement('td');
    act.className = 'actions-col';
    if (inv.status === 'draft') {
      const issue = document.createElement('button');
      issue.className = 'link';
      issue.type = 'button';
      issue.textContent = 'Issue';
      issue.addEventListener('click', () => issueInvoice(inv.id));
      const del = document.createElement('button');
      del.className = 'danger';
      del.type = 'button';
      del.textContent = 'Delete';
      del.addEventListener('click', () => removeInvoice(inv.id));
      act.append(issue, del);
    } else if (inv.status === 'issued') {
      const pay = document.createElement('button');
      pay.className = 'link';
      pay.type = 'button';
      pay.textContent = 'Mark paid';
      pay.addEventListener('click', () => markInvoicePaid(inv.id));
      act.append(pay);
    }
    tr.append(num, cust, period, total, status, act);
    tbody.appendChild(tr);
  }
  $('invoice-empty').hidden = invoices.length !== 0;
  await refreshInvoiceSummary();
}

async function refreshInvoiceSummary() {
  const s = await api.get('/invoices/summary');
  const outstanding = Object.entries(s.outstanding || {})
    .map(([cur, minor]) => `${cur} ${formatMoney(minor)}`)
    .join(', ');
  $('invoice-summary').textContent =
    `Draft ${s.draft} · Issued ${s.issued}${s.overdue ? ` (${s.overdue} overdue)` : ''} · Paid ${s.paid}` +
    (outstanding ? ` · Outstanding: ${outstanding}` : '');
}

async function markInvoicePaid(id) {
  const reference = window.prompt('Payment reference (optional):') || '';
  try {
    await api.post(`/invoices/${id}/pay`, { reference });
    announce('Invoice marked paid.');
    await refreshInvoices();
  } catch (err) {
    announce(err.message);
  }
}

async function generateInvoice(evt) {
  evt.preventDefault();
  clearFormError($('invoice-error'));
  try {
    await api.post('/invoices', {
      customer_id: $('invoice-customer').value,
      from: $('invoice-from').value,
      to: $('invoice-to').value,
    });
    announce('Draft invoice generated.');
    await refreshInvoices();
  } catch (err) {
    showFormError($('invoice-error'), err);
  }
}

async function issueInvoice(id) {
  if (!window.confirm('Issue this invoice? Its entries will be locked from editing.')) return;
  try {
    await api.post(`/invoices/${id}/issue`);
    announce('Invoice issued.');
    await refreshInvoices();
  } catch (err) {
    announce(err.message);
  }
}

async function removeInvoice(id) {
  if (!window.confirm('Delete this draft invoice?')) return;
  try {
    await api.del(`/invoices/${id}`);
    await refreshInvoices();
    announce('Invoice deleted.');
  } catch (err) {
    announce(err.message);
  }
}

// ---------------------------------------------------------------- expenses --

async function refreshCategories() {
  const data = await api.get('/categories');
  state.categories = data.categories || [];
  const ul = $('category-list');
  ul.textContent = '';
  for (const cat of state.categories) {
    const li = document.createElement('li');
    li.className = 'chip';
    li.textContent = cat.name;
    const del = document.createElement('button');
    del.type = 'button';
    del.className = 'chip-x';
    del.setAttribute('aria-label', `Delete category ${cat.name}`);
    del.textContent = '×';
    del.addEventListener('click', async () => {
      try {
        await api.del(`/categories/${cat.id}`);
        await refreshCategories();
        await fillCategorySelect();
      } catch (err) {
        announce(err.message);
      }
    });
    li.append(del);
    ul.appendChild(li);
  }
}

async function addCategory(evt) {
  evt.preventDefault();
  try {
    await api.post('/categories', { name: $('category-name').value.trim() });
    $('category-name').value = '';
    await refreshCategories();
    await fillCategorySelect();
    announce('Category added.');
  } catch (err) {
    announce(err.message);
  }
}

async function fillCategorySelect() {
  const select = $('expense-category');
  select.textContent = '';
  const ph = document.createElement('option');
  ph.value = '';
  ph.textContent = 'None';
  select.appendChild(ph);
  for (const cat of state.categories || []) {
    const opt = document.createElement('option');
    opt.value = cat.id;
    opt.textContent = cat.name;
    select.appendChild(opt);
  }
}

async function refreshExpenses() {
  const data = await api.get('/expenses');
  const expenses = data.expenses || [];
  const tbody = $('expense-table').querySelector('tbody');
  tbody.textContent = '';
  for (const x of expenses) {
    const tr = document.createElement('tr');
    const date = document.createElement('th');
    date.scope = 'row';
    date.textContent = x.date;
    const cust = document.createElement('td');
    cust.textContent = customerName(x.customer_id);
    const proj = document.createElement('td');
    proj.textContent = x.project_code || '';
    const cat = document.createElement('td');
    const c = (state.categories || []).find((k) => k.id === x.category_id);
    cat.textContent = c ? c.name : '';
    const amt = document.createElement('td');
    amt.className = 'num';
    amt.textContent = `${x.currency} ${formatMoney(x.amount_minor)}`;
    const bill = document.createElement('td');
    bill.textContent = x.billable ? 'yes' : 'no';
    const act = document.createElement('td');
    act.className = 'actions-col';
    const del = document.createElement('button');
    del.className = 'danger';
    del.type = 'button';
    del.textContent = 'Delete';
    del.addEventListener('click', async () => {
      if (!window.confirm('Delete this expense?')) return;
      await api.del(`/expenses/${x.id}`);
      await refreshExpenses();
      announce('Expense deleted.');
    });
    act.append(del);
    tr.append(date, cust, proj, cat, amt, bill, act);
    tbody.appendChild(tr);
  }
  $('expense-empty').hidden = expenses.length !== 0;
}

async function addExpense(evt) {
  evt.preventDefault();
  clearFormError($('expense-error'));
  const body = {
    date: $('expense-date').value,
    customer_id: $('expense-customer').value,
    amount_minor: Math.round(Number($('expense-amount').value) * 100),
    currency: $('expense-currency').value.trim().toUpperCase(),
    billable: $('expense-billable').checked,
    note: $('expense-note').value,
  };
  if ($('expense-project').value) body.project_code = $('expense-project').value;
  if ($('expense-category').value) body.category_id = $('expense-category').value;
  try {
    await api.post('/expenses', body);
    $('expense-amount').value = '';
    $('expense-note').value = '';
    announce('Expense added.');
    await refreshExpenses();
  } catch (err) {
    showFormError($('expense-error'), err);
  }
}

// ------------------------------------------------------------ submissions --

async function refreshSubmissions() {
  const data = await api.get('/submissions');
  const subs = data.submissions || [];
  const tbody = $('submission-table').querySelector('tbody');
  tbody.textContent = '';
  for (const s of subs) {
    const tr = document.createElement('tr');
    const week = document.createElement('th');
    week.scope = 'row';
    week.textContent = `${s.week_start} → ${s.week_end}`;
    const count = document.createElement('td');
    count.className = 'num';
    count.textContent = String(s.entry_ids.length);
    const state = document.createElement('td');
    const badge = document.createElement('span');
    badge.className = s.state === 'approved' ? 'badge on' : 'badge';
    badge.textContent = s.state;
    state.appendChild(badge);
    const comment = document.createElement('td');
    comment.textContent = s.comment || '';
    const act = document.createElement('td');
    act.className = 'actions-col';
    if (s.state === 'submitted') {
      const approve = document.createElement('button');
      approve.className = 'link';
      approve.type = 'button';
      approve.textContent = 'Approve';
      approve.addEventListener('click', () => decideSubmission(s.id, 'approve'));
      const reject = document.createElement('button');
      reject.className = 'danger';
      reject.type = 'button';
      reject.textContent = 'Reject';
      reject.addEventListener('click', () => decideSubmission(s.id, 'reject'));
      act.append(approve, reject);
    }
    tr.append(week, count, state, comment, act);
    tbody.appendChild(tr);
  }
  $('submission-empty').hidden = subs.length !== 0;
}

async function submitWeek(evt) {
  evt.preventDefault();
  clearFormError($('submission-error'));
  try {
    await api.post('/submissions', { week_start: $('submission-week').value });
    announce('Timesheet submitted.');
    await refreshSubmissions();
    await refreshDay();
  } catch (err) {
    showFormError($('submission-error'), err);
  }
}

async function decideSubmission(id, decision) {
  try {
    await api.post(`/submissions/${id}/decision`, { decision, comment: '' });
    announce(`Timesheet ${decision === 'approve' ? 'approved' : 'rejected'}.`);
    await refreshSubmissions();
  } catch (err) {
    announce(err.message);
  }
}

// --------------------------------------------------------------- settings --

async function refreshSettings() {
  try {
    const data = await api.get('/admin/secrets');
    $('vault-status').textContent = 'Credential vault: enabled (encrypted at rest).';
    $('secret-form').hidden = false;
    const tbody = $('secret-table').querySelector('tbody');
    tbody.textContent = '';
    const secrets = data.secrets || [];
    for (const s of secrets) {
      const tr = document.createElement('tr');
      const key = document.createElement('th');
      key.scope = 'row';
      key.textContent = s.key;
      const hint = document.createElement('td');
      hint.textContent = s.hint;
      const act = document.createElement('td');
      act.className = 'actions-col';
      const del = document.createElement('button');
      del.className = 'danger';
      del.type = 'button';
      del.textContent = 'Delete';
      del.addEventListener('click', () => deleteSecret(s.key));
      act.append(del);
      tr.append(key, hint, act);
      tbody.appendChild(tr);
    }
    $('secret-empty').hidden = secrets.length !== 0;
  } catch (err) {
    if (err.status === 503) {
      $('vault-status').textContent = 'Credential vault disabled — set TUCANO_SECRET_KEY to enable.';
      $('secret-form').hidden = true;
      $('secret-table').hidden = true;
    } else {
      announce(err.message);
    }
  }
}

async function addSecret(evt) {
  evt.preventDefault();
  clearFormError($('secret-error'));
  const key = $('secret-key').value.trim();
  try {
    await api.put(`/admin/secrets/${encodeURIComponent(key)}`, { value: $('secret-value').value });
    $('secret-key').value = '';
    $('secret-value').value = '';
    announce('Secret saved.');
    await refreshSettings();
  } catch (err) {
    showFormError($('secret-error'), err);
  }
}

async function deleteSecret(key) {
  if (!window.confirm(`Delete secret "${key}"?`)) return;
  try {
    await api.del(`/admin/secrets/${encodeURIComponent(key)}`);
    await refreshSettings();
    announce('Secret deleted.');
  } catch (err) {
    announce(err.message);
  }
}

// ------------------------------------------------------------------- timer --

let timerBase = 0;
let timerInterval = null;

function fmtHMS(total) {
  const h = String(Math.floor(total / 3600)).padStart(2, '0');
  const m = String(Math.floor((total % 3600) / 60)).padStart(2, '0');
  const s = String(total % 60).padStart(2, '0');
  return `${h}:${m}:${s}`;
}

function stopTimerTick() {
  if (timerInterval) {
    clearInterval(timerInterval);
    timerInterval = null;
  }
}

function startTimerTick(elapsedSeconds) {
  timerBase = elapsedSeconds;
  $('timer-display').textContent = fmtHMS(timerBase);
  stopTimerTick();
  timerInterval = setInterval(() => {
    timerBase += 1;
    $('timer-display').textContent = fmtHMS(timerBase);
  }, 1000);
}

async function refreshTimer() {
  const data = await api.get('/timer');
  const running = !!data;
  ['timer-customer', 'timer-project', 'timer-start'].forEach((id) => {
    $(id).hidden = running;
  });
  ['timer-display', 'timer-stop', 'timer-discard'].forEach((id) => {
    $(id).hidden = !running;
  });
  if (running) {
    startTimerTick(data.elapsed_seconds);
  } else {
    stopTimerTick();
  }
}

async function startTimer() {
  const customerId = $('timer-customer').value;
  const projectCode = $('timer-project').value;
  if (!customerId || !projectCode) {
    announce('Choose a customer and project first.');
    return;
  }
  try {
    await api.post('/timer', { customer_id: customerId, project_code: projectCode });
    await refreshTimer();
    announce('Timer started.');
  } catch (err) {
    announce(err.message);
  }
}

async function stopTimer() {
  try {
    await api.post('/timer/stop');
    await refreshTimer();
    await refreshDay();
    announce('Timer stopped and logged.');
  } catch (err) {
    announce(err.message);
  }
}

async function discardTimer() {
  try {
    await api.del('/timer');
    await refreshTimer();
    announce('Timer discarded.');
  } catch (err) {
    announce(err.message);
  }
}

// ---------------------------------------------------------------- boot ---

function switchTab(tabId) {
  const tab = $(tabId);
  if (tab) tab.click();
}

// Wires the app listeners and loads the first data. Called once authenticated.
async function startApp() {
  const today = isoDate(new Date());
  $('day-date').value = today;
  $('entry-date').value = today;
  $('week-date').value = today;
  $('report-from').value = mondayOf(today);
  $('report-to').value = addDays(mondayOf(today), 6);

  initTabs();

  $('day-picker').addEventListener('submit', (e) => { e.preventDefault(); refreshDay(); });
  $('entry-form').addEventListener('submit', saveEntry);
  $('entry-cancel').addEventListener('click', resetEntryForm);
  $('entry-customer').addEventListener('change', async (e) => {
    await fillProjectSelect($('entry-project'), e.target.value, null);
    fillTaskSelect($('entry-task'), '', null, null);
    $('entry-project').focus();
  });
  $('entry-project').addEventListener('change', async (e) => {
    await fillTaskSelect($('entry-task'), $('entry-customer').value, e.target.value, null);
  });

  $('week-picker').addEventListener('submit', (e) => { e.preventDefault(); refreshWeek(); });

  $('customer-form').addEventListener('submit', saveCustomer);
  $('customer-cancel').addEventListener('click', cancelCustomerEdit);
  $('project-form').addEventListener('submit', saveProject);
  $('project-cancel').addEventListener('click', cancelProjectEdit);
  $('task-form').addEventListener('submit', saveTask);
  $('task-cancel').addEventListener('click', cancelTaskEdit);

  $('report-form').addEventListener('submit', runReport);

  $('invoice-form').addEventListener('submit', generateInvoice);

  $('category-form').addEventListener('submit', addCategory);
  $('expense-form').addEventListener('submit', addExpense);
  $('submission-form').addEventListener('submit', submitWeek);
  $('secret-form').addEventListener('submit', addSecret);
  $('timer-start').addEventListener('click', startTimer);
  $('timer-stop').addEventListener('click', stopTimer);
  $('timer-discard').addEventListener('click', discardTimer);
  $('timer-customer').addEventListener('change', async (e) => {
    await fillProjectSelect($('timer-project'), e.target.value, null);
  });
  $('submission-week').value = mondayOf(today);
  $('expense-customer').addEventListener('change', async (e) => {
    await fillProjectSelect($('expense-project'), e.target.value, null);
    const c = state.customers.find((x) => x.id === e.target.value);
    if (c) $('expense-currency').value = c.currency;
  });

  await loadCustomers();
  await refreshCustomerTable();
  await refreshCustomerPickers();
  await fillProjectSelect($('entry-project'), '', null);
  await refreshDay();
  fillCustomerSelect($('invoice-customer'), '', true);
  await refreshInvoices();
  fillCustomerSelect($('expense-customer'), '', true);
  fillCustomerSelect($('timer-customer'), '', true);
  await fillProjectSelect($('timer-project'), '', null);
  await refreshTimer();
  await fillProjectSelect($('expense-project'), '', null);
  $('expense-date').value = today;
  await refreshCategories();
  await fillCategorySelect();
  await refreshExpenses();
  await refreshSubmissions();
  await refreshSettings();
  $('version').textContent = 'TucanoTime';
}

// ----------------------------------------------------------------- auth ---

let authMode = 'login';

function showAuth(mode) {
  authMode = mode;
  $('auth-overlay').hidden = false;
  if (mode === 'setup') {
    $('auth-title').textContent = 'Create the first administrator';
    $('auth-sub').textContent = 'No accounts exist yet — set up the admin login.';
    $('auth-name-field').hidden = false;
    $('auth-submit').textContent = 'Create admin';
    $('auth-password').setAttribute('autocomplete', 'new-password');
  } else {
    $('auth-title').textContent = 'Sign in';
    $('auth-sub').textContent = '';
    $('auth-name-field').hidden = true;
    $('auth-submit').textContent = 'Sign in';
    $('auth-password').setAttribute('autocomplete', 'current-password');
  }
  $('auth-email').focus();
}

function showAccount(me) {
  $('auth-overlay').hidden = true;
  $('account').hidden = false;
  $('who').textContent = me.name || me.email;
  // Settings (credential vault) is admin-only; hide the tab for members.
  const settingsTab = $('tab-settings');
  if (settingsTab) settingsTab.hidden = me.role !== 'admin';
}

async function initAuth() {
  const st = await api.get('/auth/status');
  if (!st.initialised) {
    showAuth('setup');
    return false;
  }
  try {
    const me = await api.get('/auth/me');
    showAccount(me);
    return true;
  } catch {
    showAuth('login');
    return false;
  }
}

async function onAuthSubmit(evt) {
  evt.preventDefault();
  clearFormError($('auth-error'));
  const email = $('auth-email').value.trim();
  const password = $('auth-password').value;
  try {
    const me =
      authMode === 'setup'
        ? await api.post('/auth/bootstrap', { name: $('auth-name').value.trim(), email, password })
        : await api.post('/auth/login', { email, password });
    showAccount(me);
    await startApp();
  } catch (err) {
    showFormError($('auth-error'), err);
  }
}

async function logout() {
  try {
    await api.post('/auth/logout');
  } catch {
    /* best effort */
  }
  window.location.reload();
}

document.addEventListener('DOMContentLoaded', async () => {
  $('auth-form').addEventListener('submit', onAuthSubmit);
  $('logout').addEventListener('click', logout);
  try {
    if (await initAuth()) await startApp();
  } catch (err) {
    announce(`Failed to start: ${err.message}`);
  }
});
