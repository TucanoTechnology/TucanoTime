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
  // Fetch a binary body with the session cookie intact (#113). A bare
  // <a href> would also send the cookie, but we need to inspect the status
  // and map error JSON, so this mirrors request() for the non-JSON case.
  async getBlob(url) {
    const res = await fetch(url, { method: 'GET', headers: {}, credentials: 'same-origin' });
    if (!res.ok) {
      const text = await res.text().catch(() => '');
      let msg = `HTTP ${res.status}`;
      try {
        msg = JSON.parse(text)?.error?.message || msg;
      } catch {
        /* body was not the JSON error shape */
      }
      const err = new Error(msg);
      err.status = res.status;
      throw err;
    }
    return res.blob();
  },
  get: (u) => api.request('GET', u),
  post: (u, b) => api.request('POST', u, b),
  put: (u, b) => api.request('PUT', u, b),
  del: (u) => api.request('DELETE', u),
};

const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------- helpers --
//
// `el` replaces the hand-rolled `createElement` chains the renderers copied
// around (#102): ~350 lines of identical boilerplate become one builder. As
// everywhere in this file, values enter the DOM through `textContent` only —
// never innerHTML.

function el(tag, opts = {}, children = []) {
  const node = document.createElement(tag);
  if (opts.cls) node.className = opts.cls;
  if (opts.text !== undefined) node.textContent = opts.text;
  if (opts.type) node.type = opts.type;
  if (opts.value !== undefined) node.value = opts.value;
  if (opts.attrs) {
    for (const [k, v] of Object.entries(opts.attrs)) node.setAttribute(k, v);
  }
  if (opts.data) {
    for (const [k, v] of Object.entries(opts.data)) node.dataset[k] = v;
  }
  if (opts.on) {
    for (const [evt, fn] of Object.entries(opts.on)) node.addEventListener(evt, fn);
  }
  for (const child of [].concat(children)) if (child) node.appendChild(child);
  return node;
}

// The shared select-refill pattern: clear, then append options described as
// `{ value, text, selected }`. Selection happens by property, as before.
function fillSelect(select, options) {
  select.textContent = '';
  for (const o of options) {
    const opt = el('option', { value: o.value, text: o.text });
    if (o.selected) opt.selected = true;
    select.appendChild(opt);
  }
}

// ---------------------------------------------------------------- dialogs --
//
// One inline <dialog> replaces window.confirm/prompt/alert (#102): focus is
// trapped by the native element, Enter accepts, Escape cancels, and the jsdom
// harness drives the same buttons a real user sees. Resolution is wired to
// the buttons directly (no `method="dialog"`), so behaviour does not depend
// on the host implementing dialog form submission.

function dlgSettle(result) {
  const dlg = $('app-dialog');
  const resolve = dlg.__resolve;
  if (!resolve) return; // already settled (button + Escape race)
  dlg.__resolve = null;
  if (typeof dlg.close === 'function') dlg.close();
  else dlg.removeAttribute('open');
  resolve(result);
}

function showDialog({ title, message, input = null, value = '', okLabel = 'OK', cancelLabel = null }) {
  return new Promise((resolve) => {
    $('dlg-title').textContent = title;
    $('dlg-message').textContent = message;
    const wrap = $('dlg-input-wrap');
    wrap.hidden = !input;
    if (input) $('dlg-input-label').textContent = input;
    $('dlg-input').value = value;
    const ok = $('dlg-ok');
    ok.textContent = okLabel;
    const cancel = $('dlg-cancel');
    cancel.textContent = cancelLabel || 'Cancel';
    cancel.hidden = !cancelLabel;
    const dlg = $('app-dialog');
    dlg.__resolve = resolve;
    dlg.__inputMode = !!input;
    if (typeof dlg.showModal === 'function') dlg.showModal();
    else dlg.setAttribute('open', ''); // jsdom fallback: no modal support yet
    (input ? $('dlg-input') : ok).focus();
  });
}

function askConfirm(message, okLabel = 'Confirm') {
  return showDialog({
    title: 'Please confirm',
    message,
    okLabel,
    cancelLabel: 'Cancel',
  }).then((r) => r === 'ok');
}

// Resolves with the typed string, or null when cancelled (like prompt()).
function askPrompt(message, defaultValue = '') {
  return showDialog({ title: 'Input', message, input: message, value: defaultValue }).then(
    (r) => (r === 'cancel' ? null : r),
  );
}

function askAlert(message) {
  return showDialog({ title: 'Notice', message });
}

function initDialog() {
  const dlg = $('app-dialog');
  $('dlg-ok').addEventListener('click', () => {
    dlgSettle(dlg.__inputMode ? $('dlg-input').value : 'ok');
  });
  // The promise always resolves with a settle MARKER; `askConfirm` maps it to
  // a boolean and `askPrompt` maps it to null-on-cancel, so a cancel marker
  // can never collide with a falsy user value.
  $('dlg-cancel').addEventListener('click', () => dlgSettle('cancel'));
  dlg.addEventListener('cancel', () => dlgSettle('cancel')); // native Escape
  dlg.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') dlgSettle('cancel'); // hosts without dialog keyboard
    else if (e.key === 'Enter') {
      e.preventDefault();
      $('dlg-ok').click();
    }
  });
}

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
  const dt = new Date(y, m - 1, d); // local midnight: calendar-day math with local getters
  dt.setDate(dt.getDate() + n);
  return isoDate(dt);
}

function mondayOf(dateStr) {
  const [y, m, d] = dateStr.split('-').map(Number);
  const dt = new Date(y, m - 1, d);
  const dow = dt.getDay(); // 0 Sun .. 6 Sat
  const shift = (dow === 0 ? -6 : 1) - dow;
  return addDays(dateStr, shift);
}

function formatMoney(minor) {
  return (minor / 100).toFixed(2);
}

// ----------------------------------------------------------------- cache ---

const state = {
  invoices: [], // #133: last rendered invoice list
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
  const opts = state.customers.map((c) => ({
    value: c.id,
    text: c.active ? c.name : `${c.name} (inactive)`,
    selected: c.id === selectedId,
  }));
  if (includePlaceholder) opts.unshift({ value: '', text: 'Choose a customer…' });
  fillSelect(select, opts);
}

async function fillProjectSelect(select, customerId, selectedCode) {
  if (!customerId) {
    fillSelect(select, [{ value: '', text: 'Choose a customer first…' }]);
    return;
  }
  const projects = await loadProjects(customerId);
  fillSelect(select, [
    { value: '', text: 'Choose a project…' },
    ...projects.map((p) => ({
      value: p.code,
      text: p.active ? `${p.code} — ${p.name}` : `${p.code} — ${p.name} (inactive)`,
      selected: p.code === selectedCode,
    })),
  ]);
}

// Loads a project's tasks into the given select, with a leading "None".
async function fillTaskSelect(select, customerId, projectCode, selectedCode) {
  const opts = [{ value: '', text: projectCode ? 'None' : 'Choose a project first…' }];
  if (customerId && projectCode) {
    const data = await api.get(
      `/customers/${customerId}/projects/${encodeURIComponent(projectCode)}/tasks`,
    );
    for (const t of data.tasks || []) {
      opts.push({
        value: t.code,
        text: t.active ? `${t.code} — ${t.name}` : `${t.code} — ${t.name} (inactive)`,
        selected: t.code === selectedCode,
      });
    }
  }
  fillSelect(select, opts);
}

// ------------------------------------------------------- timesheet segment --
//
// #107: Day and Week are views *inside* the Timesheets section — a second,
// horizontal tablist with its own roving tabindex (the sidebar list is
// scoped to #tabs, so the two never mix).

let segActiveId = 'ts-day';

function activateSeg(segId, refresh = true) {
  for (const id of ['ts-day', 'ts-week', 'ts-cal']) {
    const on = id === segId;
    const tab = $(id);
    tab.setAttribute('aria-selected', String(on));
    tab.tabIndex = on ? 0 : -1;
    $(tab.getAttribute('aria-controls')).hidden = !on;
  }
  segActiveId = segId;
  if (refresh) refreshTimesheetView();
}

function refreshTimesheetView() {
  const run =
    segActiveId === 'ts-day' ? refreshDay()
      : segActiveId === 'ts-cal' ? refreshTimeCal()
        : refreshWeek();
  run.catch?.(() => {});
}

function initSegTabs() {
  const segs = [$('ts-day'), $('ts-week'), $('ts-cal')];
  segs.forEach((t, i) => {
    t.addEventListener('click', () => activateSeg(t.id));
    t.addEventListener('keydown', (e) => {
      let j = null;
      if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') j = (i + 1) % segs.length;
      if (e.key === 'Home') j = 0;
      if (e.key === 'End') j = segs.length - 1;
      if (j !== null) {
        e.preventDefault();
        activateSeg(segs[j].id);
        segs[j].focus();
      }
    });
  });
}

/// Opens the Timesheets section with a given segment (used by jump-to-entry
/// and the week grid's "track time" flow).
function showTimesheet(segId) {
  if ($('tab-timesheet').getAttribute('aria-selected') !== 'true') {
    suppressPanelRefresh = true;
    $('tab-timesheet').click();
    suppressPanelRefresh = false;
  }
  activateSeg(segId, false);
}

// ------------------------------------------------------ calendar view ----
//
// #140: a month grid of LOGGED time inside the Timesheets segment group.
// Read-only by design — a day click hands over to the Day view (and its
// dialog, #145) where editing and locks already live. The imported Google/MS
// calendar events (#36) keep their own section; this never touches them.

const calState = { month: '' }; // 'YYYY-MM'; empty = follow the displayed day

function shiftMonth(month, delta) {
  const [yy, mm] = month.split('-').map(Number);
  return new Date(Date.UTC(yy, mm - 1 + delta, 1)).toISOString().slice(0, 7);
}

function monthName(month) {
  const [yy, mm] = month.split('-').map(Number);
  return `${MONTH_NAMES[mm - 1]} ${yy}`;
}

async function refreshTimeCal() {
  const today = isoDate(new Date());
  if (!calState.month) calState.month = (($('day-date').value || today).slice(0, 7));
  const month = calState.month;
  const [yy, mm] = month.split('-').map(Number);
  const last = new Date(Date.UTC(yy, mm, 0)).getUTCDate();
  const monthStart = `${month}-01`;
  const monthEnd = `${month}-${String(last).padStart(2, '0')}`;
  $('cal-label').textContent = monthName(month);
  const data = await api.get(`/entries?from=${monthStart}&to=${monthEnd}`);
  const entries = data.entries || [];
  const byDate = new Map();
  let totalHundredths = 0;
  for (const e of entries) {
    const h = Math.round(Number(e.hours) * 100);
    totalHundredths += h;
    if (!byDate.has(e.date)) byDate.set(e.date, []);
    byDate.get(e.date).push({ ...e, hundredths: h });
  }
  $('cal-total').textContent = totalHundredths
    ? `${monthName(month)}: ${fmtHM(totalHundredths)} logged`
    : `${monthName(month)}: nothing logged yet`;
  const tbody = $('cal-grid').querySelector('tbody');
  tbody.textContent = '';
  const lead = (new Date(`${monthStart}T00:00:00Z`).getUTCDay() + 6) % 7;
  let row = el('tr');
  for (let i = 0; i < lead; i += 1) row.appendChild(el('td', { cls: 'cal-out' }));
  for (let d = 1; d <= last; d += 1) {
    const date = `${month}-${String(d).padStart(2, '0')}`;
    const dayEntries = byDate.get(date) || [];
    const kids = [el('span', { cls: 'cal-num', text: String(d) })];
    if (dayEntries.length) {
      const sum = dayEntries.reduce((a, e) => a + e.hundredths, 0);
      kids.push(el('span', { cls: 'cal-hours', text: fmtHM(sum) }));
      const names = dayEntries.map((e) => e.project_code);
      const uniq = [...new Set(names)];
      for (const code of uniq.slice(0, 3)) kids.push(el('span', { cls: 'cal-chip', text: code }));
      if (uniq.length > 3) kids.push(el('span', { cls: 'cal-chip', text: `+${uniq.length - 3}` }));
    }
    const cell = el(
      'td',
      { cls: `cal-cell${dayEntries.length ? ' has' : ''}${date === today ? ' today' : ''}` },
      [
        dayEntries.length
          ? el('button', {
              type: 'button',
              cls: 'cal-day',
              on: {
                click: () => {
                  activateSeg('ts-day');
                  $('day-date').value = date;
                  refreshDay().catch?.(() => {});
                  announce(`Opened ${date} in the Day view.`);
                },
              },
              attrs: {
                'aria-label': `${d} ${monthName(month)}: ${fmtHM(
                  dayEntries.reduce((a, e) => a + e.hundredths, 0),
                )} across ${dayEntries.length} entr${dayEntries.length === 1 ? 'y' : 'ies'} — open Day view`,
              },
            }, kids)
          : el('div', { cls: 'cal-day cal-plain' }, kids),
      ],
    );
    row.appendChild(cell);
    if ((lead + d) % 7 === 0) {
      tbody.appendChild(row);
      row = el('tr');
    }
  }
  if (row.children.length) {
    while (row.children.length < 7) row.appendChild(el('td', { cls: 'cal-out' }));
    tbody.appendChild(row);
  }
}

// ---------------------------------------------------------------- tabs ---

function initTabs() {
  const tabs = Array.from(document.querySelectorAll('#tabs [role="tab"]'));
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
    // Re-pull the panel's data on show so it reflects changes made elsewhere
    // (e.g. an invoice issued via the API) without a full reload.
    refreshPanel(tab.id);
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

/// Hundredths of an hour as `H:MM` (e.g. 425 -> 4:15).
function fmtHM(hundredths) {
  const totalMin = Math.round(hundredths * 0.6);
  const h = Math.floor(totalMin / 60);
  const m = totalMin % 60;
  return `${h}:${String(m).padStart(2, '0')}`;
}

function dayDate() {
  return $('day-date').value;
}

const DAY_NAMES = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
const MONTH_NAMES = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

function setDayLabel() {
  const d = dayDate();
  const [y, m, day] = d.split('-').map(Number);
  const dt = new Date(Date.UTC(y, m - 1, day));
  // Mockup format (#108): "Monday, 28 Sep".
  $('day-label').textContent = `${DAY_NAMES[dt.getUTCDay()]}, ${day} ${MONTH_NAMES[m - 1]}`;
}

// The week strip: totals per day for the week containing the selected day.
async function refreshWeekStrip() {
  const d = dayDate();
  const start = mondayOf(d);
  const end = addDays(start, 6);
  let entries = [];
  try {
    const data = await api.get(`/entries?from=${start}&to=${end}`);
    entries = data.entries || [];
  } catch {
    $('week-strip').textContent = '';
    return;
  }
  const totals = {};
  for (const e of entries) totals[e.date] = (totals[e.date] || 0) + Math.round(e.hours * 100);
  const strip = $('week-strip');
  strip.textContent = '';
  for (let i = 0; i < 7; i += 1) {
    const date = addDays(start, i);
    const [y, m, dd] = date.split('-').map(Number);
    strip.appendChild(
      el(
        'button',
        {
          type: 'button',
          cls: 'ws-day' + (date === d ? ' selected' : ''),
          data: { date },
          attrs: date === d ? { 'aria-current': 'date' } : {},
          on: { click: () => selectDay(date) },
        },
        [
          el('span', {
            cls: 'ws-name',
            text: `${new Date(Date.UTC(y, m - 1, dd)).toUTCString().slice(0, 3)} ${dd}`,
          }),
          el('span', { cls: 'ws-total num', text: fmtHM(totals[date] || 0) }),
          el('span', { cls: 'ws-clock', text: '◷', attrs: { 'aria-hidden': 'true' } }),
        ],
      ),
    );
  }
  const sum = Object.values(totals).reduce((a, b) => a + b, 0);
  strip.appendChild(el('span', { cls: 'ws-week num', text: `Week total ${fmtHM(sum)}` }));
}

async function selectDay(date) {
  $('day-date').value = date;
  await refreshDay();
}

function navigateDay(delta) {
  const next = addDays(dayDate(), delta);
  $('day-date').value = next;
  refreshDay();
}

async function refreshDay() {
  const date = dayDate();
  if (!date) return;
  setDayLabel();
  const data = await api.get(`/entries?date=${encodeURIComponent(date)}`);
  const rows = data.entries || [];
  // #108: rows carry a lock indicator when the entry sits on an issued
  // invoice or a submitted/approved week (same lookup the grid uses, cached).
  const locked = await weekLockIds();
  const tbody = $('day-table').querySelector('tbody');
  tbody.textContent = '';
  let total = 0;
  for (const e of rows) {
    total += Math.round(e.hours * 100);
    const projName = (state.projectsByCustomer[e.customer_id] || []).find((p) => p.code === e.project_code);
    const custLine =
      customerName(e.customer_id) + (e.task_code ? ` · ${e.task_code}` : '');
    const hoursCell = el('td', { cls: 'num entry-hours' }, [
      el('span', { text: fmtHM(Math.round(e.hours * 100)) }),
      locked.has(e.id)
        ? el('span', {
            cls: 'lock',
            text: '🔒',
            attrs: {
              'aria-label': 'locked',
              title: 'On an issued invoice or submitted week — editing is blocked',
            },
          })
        : null,
    ]);
    const tr = el(
      'tr',
      { data: { entryId: e.id } }, // C7: target the exact row on jump-to-entry
      [
        el(
          'td',
          { cls: 'entry-main' },
          [
            el('div', {
              cls: 'entry-project',
              text: projName ? `${e.project_code} — ${projName.name}` : e.project_code,
            }),
            el('div', { cls: 'entry-customer', text: custLine }),
            // The non-billable badge sits after the note text, as before.
            el(
              'div',
              { cls: 'entry-note', text: e.note || '', attrs: { title: e.note || '' } },
              e.billable ? [] : [el('span', { cls: 'badge', text: ' non-billable' })],
            ),
          ],
        ),
        hoursCell,
        el('td', { cls: 'actions-col' }, [
          el('button', {
            cls: 'pill',
            type: 'button',
            text: 'Edit',
            on: { click: () => startEdit(e) },
          }),
          el('button', {
            cls: 'pill danger',
            type: 'button',
            text: 'Delete',
            on: { click: () => removeEntry(e) },
          }),
        ]),
      ],
    );
    tbody.appendChild(tr);
  }
  $('day-total').textContent = fmtHM(total);
  $('day-table').hidden = rows.length === 0;
  $('day-empty').hidden = rows.length !== 0;
  $('entry-date').value = date;
  $('entry-form-date').textContent = date ? `For ${date}` : '';
  // D2: the strip and calendar pulls are independent — fetch them at once.
  await Promise.all([refreshWeekStrip(), refreshCalendar(date)]);
}

/// Copy-forward (#12, restyled #108): pending rows for the projects worked
/// `ago` days before the current one, saved only when the user fills in hours
/// (hours must be >= 0.01 to persist).
async function copyPreviousDay(ago = 1) {
  const target = dayDate();
  const prev = addDays(target, -ago);
  const list = $('copy-rows');
  list.textContent = '';
  let rows = [];
  try {
    const data = await api.get(`/entries?date=${prev}`);
    rows = data.entries || [];
  } catch (err) {
    announce(`Could not read ${prev}: ${err.message}`);
    return;
  }
  if (rows.length === 0) {
    announce(`No entries on ${prev} to copy.`);
    return;
  }
  const seen = new Set();
  for (const e of rows) {
    const key = `${e.customer_id}|${e.project_code}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const input = el('input', {
      type: 'number',
      cls: 'copy-hours',
      attrs: {
        min: '0.01',
        max: '24',
        step: '0.01',
        'aria-label': `Hours for ${e.project_code} on ${target}`,
        placeholder: 'h',
      },
    });
    const li = el('li', { cls: 'copy-row' }, [
      el('span', {
        cls: 'copy-label',
        text: `${customerName(e.customer_id)} / ${e.project_code}`,
      }),
      input,
      el('button', {
        type: 'button',
        text: 'Save',
        on: {
          click: async () => {
            const hours = Number(input.value);
            if (!(hours >= 0.01 && hours <= 24)) {
              announce('Enter hours between 0.01 and 24.');
              input.focus();
              return;
            }
            try {
              await api.post('/entries', {
                date: target,
                customer_id: e.customer_id,
                project_code: e.project_code,
                task_code: e.task_code || null,
                hours,
                note: '',
                billable: e.billable !== false,
              });
              li.remove();
              announce(`Copied ${e.project_code} to ${target}.`);
              await refreshDay();
            } catch (err) {
              announce(`Copy failed: ${err.message}`);
            }
          },
        },
      }),
    ]);
    list.appendChild(li);
  }
  announce(`${seen.size} project row(s) ready — enter hours to copy them to ${target}.`);
}

let entryOpener = null; // focus return target when the dialog closes (#145)
let entrySaving = false; // guards Escape/close while a save is in flight

/// The Day entry form is a modal dialog now (#145): open records the opener
/// so closing restores focus; jsdom lacks showModal so the attribute
/// fallback (same pattern as #102/#111) keeps the harness honest.
function showEntryForm(show) {
  const dlg = $('entry-dialog');
  if (show) {
    if (!(dlg.open === true || dlg.hasAttribute('open'))) entryOpener = document.activeElement;
    if (typeof dlg.showModal === 'function') dlg.showModal();
    else dlg.setAttribute('open', '');
    return;
  }
  if (dlg.open === true || dlg.hasAttribute('open')) {
    if (typeof dlg.close === 'function') dlg.close();
    else dlg.removeAttribute('open');
  }
  if (entryOpener && document.contains(entryOpener) && typeof entryOpener.focus === 'function') {
    entryOpener.focus();
  }
  entryOpener = null;
}

function startEdit(e) {
  showEntryForm(true);
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
  $('entry-hours').focus(); // dialog is open — start where the edit happens
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
  if (!(await askConfirm(`Delete this entry (${label})? This cannot be undone.`))) return;
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
  entrySaving = true;
  try {
    if (id) {
      await api.put(`/entries/${id}`, body);
      announce('Entry updated.');
    } else {
      await api.post('/entries', body);
      announce('Entry added.');
    }
    // #145: saving returns to the timesheet — the dialog closes for creates
    // too; Track time re-opens it with the day kept for the next entry.
    const wasEdit = Boolean(id);
    const savedDate = body.date;
    // Keep the table aligned with whatever day was just written, even if it
    // differs from the one being viewed.
    $('day-date').value = savedDate;
    resetEntryForm();
    showEntryForm(false);
    if (!wasEdit) {
      // Fast re-track: reopen primed for the same day (Harvest behaviour).
      $('entry-date').value = savedDate;
      showEntryForm(true);
      $('entry-hours').focus();
    } else {
      $('day-add').focus();
    }
    await refreshDay();
  } catch (err) {
    showFormError($('entry-error'), err);
  } finally {
    entrySaving = false;
  }
}

// ---------------------------------------------------------------- week ---
// Spreadsheet-style grid (#13): one row per customer+project, Mon–Sun cells
// are editable hour inputs. Blur/Enter saves through the API; clearing a
// cell deletes the entry after confirm. Extra rows (add-row / copy-last-week)
// live client-side until their cells get hours.

const weekState = {
  extraRows: [], // [{customer_id, project_code, task_code, week}] — week-scoped (#128/#137)
  rowKeys: new Set(), // `${customer_id}::${project_code}` shown in this week (#127)
  days: [],
  seq: 0, // render guard: only the newest fetch may touch the DOM
};

// Entry ids locked by submitted weeks or issued invoices. Fetched fresh on
// every grid render — locks can appear while the grid is open (#8/#16).
let weekLockCache = { at: 0, ids: null }; // D2: 5s TTL — one commit reuses it

async function weekLockIds() {
  const nowMs = Date.now();
  if (weekLockCache.ids && nowMs - weekLockCache.at < 5000) return weekLockCache.ids;
  const locked = new Set();
  try {
    const subs = await api.get('/submissions');
    for (const s of subs.submissions || []) {
      if (s.state === 'submitted' || s.state === 'approved') {
        for (const id of s.entry_ids || []) locked.add(id);
      }
    }
  } catch { /* ignore */ }
  try {
    const invs = await api.get('/invoices'); // admin-only; members skip silently
    for (const i of invs.invoices || []) {
      if (i.status === 'issued') for (const l of i.lines || []) if (l.entry_id) locked.add(l.entry_id);
    }
  } catch { /* members cannot list invoices */ }
  weekLockCache = { at: Date.now(), ids: locked };
  return locked;
}

/// #108: the day view now shares this cache, so a mutation that changes lock
/// state (submit/decide/issue/pay) must drop it — otherwise a refresh within
/// the TTL renders stale locks.
function invalidateLockCache() {
  weekLockCache = { at: 0, ids: null };
}

function weekShort(dateStr) {
  const [y, m, d] = dateStr.split('-').map(Number);
  const dt = new Date(y, m - 1, d);
  return `${String(d).padStart(2, '0')} ${['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'][m - 1]}`;
}

async function refreshWeek() {
  const anchor = $('week-date').value;
  if (!anchor) return;
  const seq = ++weekState.seq;
  const start = mondayOf(anchor);
  $('week-date').value = start;
  const days = [];
  for (let i = 0; i < 7; i += 1) days.push(addDays(start, i));
  weekState.days = days;
  const end = days[6];
  const thisWeek = start === mondayOf(isoDate(new Date()));
  const today = isoDate(new Date());
  $('week-label').textContent = `${thisWeek ? 'This week ' : ''}${weekShort(start)} – ${weekShort(end)} ${end.slice(0, 4)}`;

  const data = await api.get(`/entries?from=${start}&to=${end}`);
  const entries = data.entries || [];
  const locked = await weekLockIds();
  if (seq !== weekState.seq) return; // a newer render superseded this one

  // #137: a line is identified by customer + project + TASK (empty task =
  // project-level), so tasks of one project no longer collapse into a row.
  const keyFor = (e) => `${e.customer_id}::${e.project_code}::${e.task_code || ''}`;
  const byKey = new Map();
  const rowFor = (customerId, projectCode, taskCode) => {
    const k = `${customerId}::${projectCode}::${taskCode || ''}`;
    if (!byKey.has(k))
      byKey.set(k, { customer_id: customerId, project_code: projectCode, task_code: taskCode || '', cells: {}, ids: {}, notes: {} });
    return byKey.get(k);
  };
  for (const e of entries) {
    const row = rowFor(e.customer_id, e.project_code, e.task_code || '');
    row.cells[e.date] = (row.cells[e.date] || 0) + Math.round(e.hours * 100);
    (row.ids[e.date] = row.ids[e.date] || []).push(e.id);
    if (e.note) row.notes[e.date] = e.note;
  }
  for (const r of weekState.extraRows)
    if (r.week === start) rowFor(r.customer_id, r.project_code, r.task_code);
  weekState.rowKeys = new Set(byKey.keys()); // what "Add row" must not offer again (#127)

  const table = $('week-table');
  table.textContent = '';

  const htr = el('tr', {}, [
    el('th', { attrs: { scope: 'col' }, text: 'Project' }),
    ...days.map((d) => {
      const [yy, mm, dd] = d.split('-').map(Number);
      return el(
        'th',
        {
          attrs: { scope: 'col' },
          cls: d === today ? 'today' : '',
          data: { date: d },
        },
        [
          el('span', { cls: 'wk-clock', text: '◷', attrs: { 'aria-hidden': 'true' } }),
          el('span', { cls: 'wk-dow', text: new Date(d).toUTCString().slice(0, 3) }),
          el('span', { cls: 'wk-date', text: `${dd} ${MONTH_NAMES[mm - 1]}` }),
        ],
      );
    }),
    el('th', { attrs: { scope: 'col' }, cls: 'num', text: 'Week' }),
  ]);
  table.appendChild(el('thead', {}, [htr]));

  const tbody = el('tbody');
  const sorted = Array.from(byKey.values()).sort(
    (a, b) =>
      customerName(a.customer_id).localeCompare(customerName(b.customer_id)) ||
      a.project_code.localeCompare(b.project_code) ||
      a.task_code.localeCompare(b.task_code),
  );
  const dayTotals = days.map(() => 0);
  for (const row of sorted) {
    let weekTotal = 0;
    let anyLocked = false;
    const projects = state.projectsByCustomer[row.customer_id] || [];
    const projName = projects.find((p) => p.code === row.project_code);
    const cells = days.map((d, i) => {
      const hundredths = row.cells[d];
      const ids = row.ids[d] || [];
      if (hundredths) {
        weekTotal += hundredths;
        dayTotals[i] += hundredths;
      }
      const isLocked = ids.some((id) => locked.has(id));
      if (isLocked) anyLocked = true;
      const input = el('input', {
        type: 'number',
        cls: 'cell-input',
        value: hundredths ? (hundredths / 100).toFixed(2) : '',
        attrs: {
          min: '0.01',
          max: '24',
          step: '0.01',
          inputmode: 'decimal',
          'aria-label': `${row.project_code}${row.task_code ? ' / ' + row.task_code : ''}, ${customerName(row.customer_id)}, ${d}: hours`,
          ...(isLocked ? { disabled: '' } : {}),
        },
        data: {
          customer: row.customer_id,
          project: row.project_code,
          task: row.task_code,
          date: d,
          ids: ids.join(','),
          was: hundredths ? (hundredths / 100).toFixed(2) : '',
        },
      });
      const children = [input];
      // #126: cells that hold an entry always show the note affordance —
      // filled ¶ with a note, faint ✎ to add one — editing IN the grid via
      // the #102 dialog instead of jumping to the day view.
      if (ids.length > 0) {
        const hasNote = Boolean(row.notes[d]);
        children.push(
          el('button', {
            type: 'button',
            cls: hasNote ? 'note-flag' : 'note-flag note-empty',
            text: hasNote ? '¶' : '✎',
            attrs: {
              title: row.notes[d] || 'Add a note',
              'aria-label': hasNote
                ? `Note: ${row.notes[d]}. Activate to edit.`
                : `Add a note for ${row.project_code} on ${d}`,
            },
            on: { click: () => editCellNote(ids[0], isLocked) },
          }),
        );
      }
      return el(
        'td',
        { cls: `cell${hundredths ? ' has' : ''}${isLocked ? ' locked' : ''}` },
        children,
      );
    });
    const tr = el(
      'tr',
      { cls: anyLocked ? 'has-lock' : '' },
      [
        el('th', { attrs: { scope: 'row' }, cls: 'row-band' }, [
          el('div', {
            cls: 'entry-project',
            text: `${row.project_code}${row.task_code ? ' · ' + row.task_code : ''}${projName && !row.task_code ? ` — ${projName.name}` : ''}`,
            attrs: row.task_code ? { title: `${row.project_code} / ${row.task_code}` } : {},
          }),
          el('div', { cls: 'entry-customer', text: customerName(row.customer_id) }),
        ]),
        ...cells,
        el('td', { cls: 'num row-total' }, [
          el('span', { text: fmtHM(weekTotal) }),
          anyLocked
            ? el('span', {
                cls: 'lock',
                text: '🔒',
                attrs: { 'aria-label': 'locked', title: 'This row holds locked entries' },
              })
            : null,
        ]),
      ],
    );
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);

  const grandRow = el('tr', {}, [
    el('th', { attrs: { scope: 'row' }, text: 'Day totals' }),
    ...dayTotals.map((t) => el('td', { cls: 'num day-total', text: t ? fmtHM(t) : '0' })),
    el('td', { cls: 'num week-total', text: fmtHM(dayTotals.reduce((a, b) => a + b, 0)) }),
  ]);
  table.appendChild(el('tfoot', {}, [grandRow]));

  $('week-table').hidden = sorted.length === 0;
  $('week-empty').hidden = sorted.length !== 0;
}

/// Inline cell save (#13): blur or Enter commits the typed hours.
async function commitCell(input) {
  const raw = input.value.trim();
  const was = input.dataset.was || '';
  if (raw === was) return;
  const customerId = input.dataset.customer;
  const projectCode = input.dataset.project;
  const date = input.dataset.date;
  const ids = (input.dataset.ids || '').split(',').filter(Boolean);
  try {
    if (raw === '') {
      if (ids.length === 0) return;
      const label = `${was}h on ${projectCode}, ${date}`;
      if (!(await askConfirm(`Delete this entry (${label})?`))) {
        input.value = was;
        return;
      }
      for (const id of ids) await api.del(`/entries/${id}`);
      announce(`Deleted ${label}.`);
    } else if (ids.length === 1) {
      const existing = await api.get(`/entries/${ids[0]}`);
      await api.put(`/entries/${ids[0]}`, {
        date,
        customer_id: customerId,
        project_code: projectCode,
        task_code: existing.task_code || null,
        hours: Number(raw),
        note: existing.note || '',
        billable: existing.billable !== false,
      });
      announce(`Saved ${raw}h — ${projectCode}, ${date}.`);
    } else if (ids.length > 1) {
      const merge = await askConfirm(
        `This cell combines ${ids.length} entries for ${projectCode} on ${date}. Saving sets the first to ${raw}h and removes the others.`,
      );
      if (!merge) {
        input.value = was;
        return;
      }
      const existing = await api.get(`/entries/${ids[0]}`);
      await api.put(`/entries/${ids[0]}`, {
        date,
        customer_id: customerId,
        project_code: projectCode,
        task_code: existing.task_code || null,
        hours: Number(raw),
        note: existing.note || '',
        billable: existing.billable !== false,
      });
      for (const id of ids.slice(1)) await api.del(`/entries/${id}`);
      announce(`Merged ${projectCode} on ${date} to ${raw}h.`);
    } else {
      await api.post('/entries', {
        date,
        customer_id: customerId,
        project_code: projectCode,
        task_code: input.dataset.task || null,
        hours: Number(raw),
        note: '',
        billable: true,
      });
      announce(`Added ${raw}h — ${projectCode}${input.dataset.task ? ' / ' + input.dataset.task : ''}, ${date}.`);
    }
    await refreshWeek();
  } catch (err) {
    // Keep the typed value visible; surface the error without losing work.
    announce(`Save failed: ${err.message}`);
    input.focus();
    try {
      await refreshWeek();
      const again = [...document.querySelectorAll('#week-table input.cell-input')].find(
        (i) =>
          i.dataset.customer === customerId && i.dataset.project === projectCode &&
          (i.dataset.task || '') === (input.dataset.task || '') && i.dataset.date === date,
      );
      if (again) {
        again.value = raw;
        again.focus();
      }
    } catch { /* grid refresh failure is secondary */ }
  }
}

function navigateWeek(delta) {
  $('week-date').value = addDays($('week-date').value, delta * 7);
  refreshWeek();
}

function toggleWeekAddRow(show) {
  $('week-add-project').hidden = !show;
  $('week-add-task').hidden = !show;
  $('week-add-confirm').hidden = !show;
  $('week-add-cancel').hidden = !show;
  if (show) $('week-add-project').focus();
}

/// Task options for the project chosen in the Add-row picker (#137). The
/// first entry keeps project-level rows possible.
async function fillWeekTaskSelect() {
  const sel = $('week-add-task');
  const [cid, code] = ($('week-add-project').value || '|').split('|');
  if (!cid || !code) {
    sel.textContent = '';
    return;
  }
  await fillTaskSelect(sel, cid, code, null);
  // relabel the generic "None" so project-level rows read clearly
  if (sel.options.length) sel.options[0].text = '— project-level (no task) —';
}

async function fillWeekProjectSelect() {
  const options = [];
  for (const c of state.customers) {
    if (!c.active) continue;
    const projects = await loadProjects(c.id);
    for (const p of projects) {
      if (!p.active) continue;
      // #127: a project that already has a line in this week is not offered —
      // confirming it before added nothing yet still claimed "Row added".
      if (weekState.rowKeys.has(`${c.id}::${p.code}::`)) continue; // project-level row shown
      options.push({ value: `${c.id}|${p.code}`, text: `${c.name} / ${p.code} — ${p.name}` });
    }
  }
  if (options.length === 0) {
    fillSelect($('week-add-project'), [
      { value: '', text: 'All active projects are already in this week' },
    ]);
  } else {
    fillSelect($('week-add-project'), options);
  }
}

/// Append an extra grid row for the displayed week; returns false when the
/// line already exists (#127/#128 — the row set belongs to one week).
function addWeekRow(customerId, projectCode, taskCode, week) {
  if (
    weekState.extraRows.some(
      (r) =>
        r.week === week &&
        r.customer_id === customerId &&
        r.project_code === projectCode &&
        (r.task_code || '') === (taskCode || ''),
    )
  ) {
    return false;
  }
  weekState.extraRows.push({ customer_id: customerId, project_code: projectCode, task_code: taskCode || '', week });
  return true;
}

/// Copy the source week's PROJECT LINES into the displayed week — never the
/// time entries (#128). Rows are week-scoped, re-copying is idempotent, and
/// the announcement reports what actually happened.
/// In-place note editing for week-grid cells (#126). The dialog pre-fills
/// the current note; save PUTs the entry with its hours untouched; cancel
/// leaves the record unchanged (never a partial write). Locked entries are
/// read-only — the note is shown, not edited, matching the cell's lock.
async function editCellNote(id, isLocked) {
  try {
    const e = await api.get(`/entries/${id}`);
    if (isLocked) {
      await askAlert(
        e.note
          ? `Locked entry note (${e.project_code}, ${e.date}): ${e.note}`
          : `This entry is locked (issued invoice or submitted week), so no note can be added.`,
      );
      return;
    }
    const note = await askPrompt(`Note for ${e.project_code} on ${e.date}:`, e.note || '');
    if (note === null) {
      announce('Note unchanged.');
      return;
    }
    await api.put(`/entries/${id}`, {
      date: e.date,
      customer_id: e.customer_id,
      project_code: e.project_code,
      task_code: e.task_code || null,
      hours: e.hours,
      note: note.trim(),
      billable: e.billable !== false,
    });
    announce('Note saved.');
    await refreshWeek();
  } catch (err) {
    announce(`Note failed: ${err.message}`);
  }
}

async function copyLastWeek(weeksAgo = 1) {
  const targetWeek = $('week-date').value; // Monday of the displayed week
  const start = addDays(targetWeek, -7 * weeksAgo);
  const end = addDays(start, 6);
  try {
    const data = await api.get(`/entries?from=${start}&to=${end}`);
    const seen = new Set();
    let added = 0;
    for (const e of data.entries || []) {
      const k = `${e.customer_id}|${e.project_code}|${e.task_code || ''}`;
      if (seen.has(k)) continue;
      seen.add(k);
      if (addWeekRow(e.customer_id, e.project_code, e.task_code || '', targetWeek)) added += 1;
    }
    await refreshWeek();
    if (seen.size === 0) {
      announce(`Nothing to copy: no project lines in the week of ${start}.`);
    } else if (added === 0) {
      announce(`Already copied: all ${seen.size} project row(s) from ${start} are in this week. No hours copied.`);
    } else {
      announce(`Copied ${added} project row(s) from ${start} — hours left blank, nothing saved yet.`);
    }
  } catch (err) {
    announce(`Copy failed: ${err.message}`);
  }
}

async function jumpToEntry(entryId) {
  const e = await api.get(`/entries/${entryId}`);
  showTimesheet('ts-day'); // suppressed — we await the refresh ourselves
  $('day-date').value = e.date;
  await refreshDay();
  startEdit(e);
  const target = document.querySelector(`#day-table tbody tr[data-entry-id="${entryId}"]`);
  if (target) target.scrollIntoView({ behavior: 'smooth' });
}

// ---------------------------------------------------------- customers ---

async function refreshCustomerTable() {
  const tbody = $('customer-table').querySelector('tbody');
  tbody.textContent = '';
  for (const c of state.customers) {
    tbody.appendChild(
      el('tr', {}, [
        el('th', { attrs: { scope: 'row' }, text: c.name }),
        el('td', { text: c.currency }),
        el('td', { cls: 'num', text: `${c.currency} ${formatMoney(c.default_rate_minor)}` }),
        el('td', {}, [
          el('span', {
            cls: c.active ? 'badge on' : 'badge',
            text: c.active ? 'Active' : 'Inactive',
          }),
        ]),
        el('td', { cls: 'actions-col' }, [
          el('button', {
            cls: 'link',
            type: 'button',
            text: 'Projects',
            on: { click: () => selectCustomerForProjects(c.id) },
          }),
          el('button', {
            cls: 'link',
            type: 'button',
            text: 'Edit',
            on: { click: () => startCustomerEdit(c) },
          }),
          el('button', {
            cls: 'danger',
            type: 'button',
            text: 'Delete',
            on: { click: () => removeCustomer(c) },
          }),
        ]),
      ]),
    );
  }
}

function startCustomerEdit(c) {
  $('customer-id').value = c.id;
  $('customer-name').value = c.name;
  $('customer-currency').value = c.currency;
  $('customer-rate').value = (c.default_rate_minor / 100).toFixed(2);
  $('customer-active').checked = c.active;
  $('customer-email').value = c.email || '';
  // Invoice document fields (#116).
  const terms = c.payment_terms || null;
  $('customer-terms').value = terms ? terms.kind : '';
  $('customer-terms-days').value = terms && terms.days != null ? terms.days : '';
  $('customer-terms-days-field').hidden = !terms || terms.kind !== 'custom';
  $('customer-subject').value = c.invoice_subject || '';
  $('customer-notes').value = c.invoice_notes || '';
  const a = c.address || {};
  $('cust-street').value = a.street || '';
  $('cust-city').value = a.city || '';
  $('cust-postal').value = a.postal_code || '';
  $('cust-country').value = a.country || '';
  $('cust-tax').value = c.tax_hundredths ? (c.tax_hundredths / 100).toFixed(2) : '0';
  $('cust-discount').value = c.discount_hundredths ? (c.discount_hundredths / 100).toFixed(2) : '0';
  setContacts(c.contacts || []);
  $('customer-save').textContent = 'Update customer';
  $('customer-cancel').hidden = false;
  clearFormError($('customer-error'));
  $('customer-name').focus();
}

// ---- customer contacts editor (#139) — rows are plain inputs read on save.
function contactRow(c = { name: '', role: '', email: '', billing: false }) {
  const mk = (ph, val, type = 'text') =>
    el('input', { attrs: { placeholder: ph, value: val, type, maxlength: ph === 'Email' ? 200 : 120 } });
  const name = mk('Name', c.name);
  const role = mk('Role', c.role);
  const email = mk('Email', c.email, 'email');
  const box = el('input', { attrs: { type: 'checkbox' } });
  box.checked = !!c.billing;
  const row = el('div', { cls: 'contact-row' }, [
    name,
    role,
    email,
    el('label', { cls: 'contact-billing' }, [box, el('span', { text: ' billing' })]),
    el('button', {
      type: 'button', cls: 'link danger', text: 'Remove',
      on: { click: () => row.remove() },
    }),
  ]);
  row.__read = () => ({
    name: name.value.trim(), role: role.value.trim(), email: email.value.trim(), billing: box.checked,
  });
  return row;
}

function readContacts() {
  return [...$('contact-rows').children].map((r) => r.__read()).filter((c) => c.name || c.email);
}

function setContacts(list) {
  $('contact-rows').textContent = '';
  for (const c of list) $('contact-rows').appendChild(contactRow(c));
}

async function saveCustomer(evt) {
  evt.preventDefault();
  clearFormError($('customer-error'));
  const id = $('customer-id').value;
  // Payment terms (#116): the select carries the kind; only `custom` sends days.
  const kind = $('customer-terms').value;
  let payment_terms = null;
  if (kind) {
    payment_terms = { kind };
    if (kind === 'custom') payment_terms.days = Number($('customer-terms-days').value);
  }
  const street = $('cust-street').value.trim();
  const city = $('cust-city').value.trim();
  const postal = $('cust-postal').value.trim();
  const country = $('cust-country').value.trim();
  const body = {
    name: $('customer-name').value.trim(),
    currency: $('customer-currency').value.trim().toUpperCase(),
    default_rate_minor: Math.round(Number($('customer-rate').value) * 100),
    active: $('customer-active').checked,
    email: $('customer-email').value.trim(),
    payment_terms,
    invoice_subject: $('customer-subject').value.trim(),
    invoice_notes: $('customer-notes').value.trim(),
    // #139: percent inputs are decimal but persist as integer hundredths.
    address: street || city || postal || country ? { street, city, postal_code: postal, country } : null,
    contacts: readContacts(),
    tax_hundredths: Math.round(Number($('cust-tax').value || 0) * 100),
    discount_hundredths: Math.round(Number($('cust-discount').value || 0) * 100),
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
  $('customer-terms-days-field').hidden = true;
  setContacts([]);
  $('customer-save').textContent = 'Add customer';
  $('customer-cancel').hidden = true;
  clearFormError($('customer-error'));
}

async function removeCustomer(c) {
  if (!(await askConfirm(`Delete customer "${c.name}"?`))) return;
  try {
    await api.del(`/customers/${c.id}`);
    await loadCustomers();
    await refreshCustomerTable();
    await refreshCustomerPickers();
    // C7: reset the project panel — it used to keep pointing at the deleted
    // customer, so the next project save 404'd, and leak its cache entry.
    if (state.selectedCustomerId === c.id) {
      state.selectedCustomerId = null;
      $('project-form').hidden = true;
      $('project-table').hidden = true;
      $('task-form').hidden = true;
      $('task-table').hidden = true;
      $('project-customer-label').textContent = '— select a customer —';
    }
    delete state.projectsByCustomer[c.id];
    announce('Customer deleted.');
  } catch (err) {
    announce(err.message);
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
    tbody.appendChild(
      el('tr', {}, [
        el('th', { attrs: { scope: 'row' }, text: p.code }),
        el('td', { text: p.name }),
        el('td', { text: p.currency || 'customer' }),
        el('td', { cls: 'num', text: p.rate_minor != null ? formatMoney(p.rate_minor) : 'customer' }),
        el('td', {}, [
          el('span', {
            cls: p.active ? 'badge on' : 'badge',
            text: p.active ? 'Active' : 'Inactive',
          }),
        ]),
        el('td', { cls: 'actions-col' }, [
          el('button', {
            cls: 'link',
            type: 'button',
            text: 'Tasks',
            on: { click: () => selectProjectForTasks(p.code) },
          }),
          el('button', {
            cls: 'link',
            type: 'button',
            text: 'Edit',
            on: { click: () => startProjectEdit(p) },
          }),
          el('button', {
            cls: 'danger',
            type: 'button',
            text: 'Delete',
            on: { click: () => removeProject(p) },
          }),
        ]),
      ]),
    );
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
  if (!(await askConfirm(`Delete project ${p.code}?`))) return;
  const cid = state.selectedCustomerId;
  try {
    await api.del(`/customers/${cid}/projects/${encodeURIComponent(p.code)}`);
    await refreshProjectTable();
    announce('Project deleted.');
  } catch (err) {
    announce(err.message);
    await askAlert(err.message);
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
    tbody.appendChild(
      el('tr', {}, [
        el('th', { attrs: { scope: 'row' }, text: t.code }),
        el('td', { text: t.name }),
        el('td', { text: t.currency || 'project' }),
        el('td', { cls: 'num', text: t.rate_minor != null ? formatMoney(t.rate_minor) : 'project' }),
        el('td', {}, [
          el('span', {
            cls: t.active ? 'badge on' : 'badge',
            text: t.active ? 'Active' : 'Inactive',
          }),
        ]),
        el('td', { cls: 'actions-col' }, [
          el('button', {
            cls: 'link',
            type: 'button',
            text: 'Edit',
            on: { click: () => startTaskEdit(t) },
          }),
          el('button', {
            cls: 'danger',
            type: 'button',
            text: 'Delete',
            on: { click: () => removeTask(t) },
          }),
        ]),
      ]),
    );
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
  if (!(await askConfirm(`Delete task ${t.code}?`))) return;
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
    await askAlert(err.message);
  }
}

async function refreshCustomerPickers() {
  // C7: every customer picker (day form, invoices, expenses, timer) refreshes
  // from one place — previously a newly added customer could not be invoiced
  // until a full page reload.
  for (const id of ['entry-customer', 'invoice-customer', 'expense-customer', 'timer-customer', 'rec-customer', 'ed-customer', 'iw-customer']) {
    const picker = $(id);
    if (picker) fillCustomerSelect(picker, picker.value, true);
  }
  const keep = $('entry-customer').value;
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
    tbody.appendChild(
      el('tr', {}, [
        el('th', { attrs: { scope: 'row' }, text: row.label }),
        el('td', { text: row.currency }),
        el('td', { cls: 'num', text: row.hours.toFixed(2) }),
        el('td', { cls: 'num', text: `${row.currency} ${formatMoney(row.amount_minor)}` }),
        el('td', { cls: 'num', text: String(row.entries) }),
      ]),
    );
  }
  $('report-total').textContent = `${summary.total_hours.toFixed(2)}  (billable ${summary.billable_hours.toFixed(2)} / non-billable ${summary.nonbillable_hours.toFixed(2)})`;
  const link = $('csv-link');
  link.href = `/reports/export.csv?from=${from}&to=${to}`;
  link.hidden = false;
  announce(`Report ready: ${summary.rows.length} rows.`);
}

// -------------------------------------------------------------- invoices ---

let invoicesRenderSeq = 0; // last-call-wins (same guard the week grid uses)

async function refreshInvoices() {
  const seq = ++invoicesRenderSeq;
  const data = await api.get('/invoices');
  if (seq !== invoicesRenderSeq) return; // a newer refresh superseded this one
  const invoices = data.invoices || [];
  state.invoices = invoices; // #133: the preview re-renders against fresh data
  const tbody = $('invoice-table').querySelector('tbody');
  tbody.textContent = '';
  const action = (cls, text, fn, title) =>
    el('button', {
      cls,
      type: 'button',
      text,
      attrs: title ? { title } : {},
      on: { click: fn },
    });
  for (const inv of invoices) {
    const actions = [];
    // Balance semantics (#114): legacy paid docs without a ledger count as
    // fully settled; a written-off balance is forgiven, not outstanding.
    const paidMinor = (inv.payments || []).reduce((a, p) => a + p.amount_minor, 0);
    const balance = inv.status === 'paid' || inv.status === 'written_off'
      ? 0
      : Math.max(inv.total_minor - paidMinor, 0);
    const open = inv.status === 'issued' || inv.status === 'partly_paid';
    if (inv.status === 'draft') {
      actions.push(
        action('link', 'Edit', () => api.get(`/invoices/${inv.id}`).then(edOpen, (e) => announce(e.message)),
          'Edit the draft lines and percentages (#143)'),
        action('link', 'Issue', () => issueInvoice(inv)),
        action('danger', 'Delete', () => removeInvoice(inv.id)),
      );
    } else if (open) {
      actions.push(
        action('link', 'Record payment', () => recordPayment(inv, balance),
          'Record a full or partial payment (partly paid keeps the rest open)'),

        action('link', 'PDF', () => downloadInvoicePdf(inv.id, inv.number),
          'Download the archived invoice PDF'),
        action('link', 'Email', () => emailInvoice(inv.id),
          'Send this invoice (PDF attached) to the customer billing email'),
        action('link', 'Email copy', () => emailInvoiceCopy(inv.id),
          'Send a PDF copy to another address, e.g. the accountant'),
        action('danger', 'Write off', () => writeOffInvoice(inv, balance),
          'Forgive the remaining balance (final; excluded from outstanding)'),
      );
    } else if (inv.status === 'paid') {
      actions.push(
        action('link', 'PDF', () => downloadInvoicePdf(inv.id, inv.number),
          'Download the archived invoice PDF'),
        action('link', 'Email', () => emailInvoice(inv.id),
          'Send this invoice (PDF attached) to the customer billing email'),
        action('link', 'Email copy', () => emailInvoiceCopy(inv.id),
          'Send a PDF copy to another address, e.g. the accountant'),
      );
    }
    // #133: selecting the row (its Preview button, a click anywhere on the
    // row, or Enter with the row focused) opens the live document preview.
    actions.unshift(
      action('link', 'Preview', (evt) => openInvoicePreview(inv, evt.currentTarget),
        'Preview this invoice document'),
    );
    tbody.appendChild(
      el('tr', {
        attrs: { tabindex: '0', 'aria-label': `Invoice ${inv.number} — activate to preview` },
        on: {
          click: (e) => {
            if (!e.target.closest('button')) openInvoicePreview(inv, null);
          },
          keydown: (e) => {
            if (e.key === 'Enter') openInvoicePreview(inv, e.currentTarget);
          },
        },
      }, [
        el('th', { attrs: { scope: 'row' }, text: inv.number }),
        el('td', { text: customerName(inv.customer_id) }),
        el('td', { text: `${inv.period_from} → ${inv.period_to}` }),
        el('td', { cls: 'num', text: `${inv.currency} ${formatMoney(inv.total_minor)}` }),
        el('td', { cls: 'num', text: open ? `${inv.currency} ${formatMoney(balance)}` : '\u2014' }),
        el('td', {}, [
          el('span', { cls: open ? 'badge on' : 'badge', text: inv.status.replace('_', ' ') }),
        ]),
        el('td', { cls: 'actions-col' }, actions),
      ]),
    );
  }
  $('invoice-empty').hidden = invoices.length !== 0;
  if (invoicePreview) {
    const fresh = invoices.find((i) => i.id === invoicePreview.id);
    if (fresh) {
      const opener = invoicePreview.opener;
      openInvoicePreview({ ...fresh }, opener && document.contains(opener) ? opener : null);
    } else {
      closeInvoicePreview();
    }
  }
  await refreshInvoiceSummary();
}

async function refreshInvoiceSummary() {
  const s = await api.get('/invoices/summary');
  const outstanding = Object.entries(s.outstanding || {})
    .map(([cur, minor]) => `${cur} ${formatMoney(minor)}`)
    .join(', ');
  $('invoice-summary').textContent =
    `Draft ${s.draft} · Issued ${s.issued}${s.overdue ? ` (${s.overdue} overdue)` : ''}` +
    (s.partly_paid ? ` · Partly paid ${s.partly_paid}` : '') +
    ` · Paid ${s.paid}` +
    (s.written_off ? ` · Written off ${s.written_off}` : '') +
    (outstanding ? ` · Outstanding: ${outstanding}` : '');
}

/// Record a full or partial payment (#114). The amount is prefilled with the
/// outstanding balance; paying the exact balance settles the invoice.
async function recordPayment(inv, balance) {
  const amount = await askPrompt(
    `Amount to record (${inv.currency} ${formatMoney(balance)} outstanding):`,
    formatMoney(balance),
  );
  if (amount === null) return; // dialog dismissed
  const trimmed = String(amount).trim();
  if (!trimmed) {
    announce('Record payment cancelled.');
    return;
  }
  const minor = Math.round(Number(trimmed) * 100);
  if (!Number.isFinite(minor) || minor <= 0) {
    announce('Enter a positive amount, e.g. 150.00.');
    return;
  }
  const reference = (await askPrompt('Payment reference (optional):')) || '';
  try {
    const paid = await api.post(`/invoices/${inv.id}/pay`, { reference, amount_minor: minor });
    invalidateLockCache(); // full settlement releases the invoice's entry locks
    if (paid.status === 'paid') {
      announce('Invoice marked paid.');
    } else {
      const rest = Math.max(
        paid.total_minor - (paid.payments || []).reduce((a, p) => a + p.amount_minor, 0),
        0,
      );
      announce(`Payment recorded. ${paid.currency} ${formatMoney(rest)} still outstanding.`);
    }
    await refreshInvoices();
  } catch (err) {
    announce(err.message);
  }
}

/// Forgive the remaining balance (#114): the reason is required and the
/// state is final, so a prompt + confirm gate the action.
async function writeOffInvoice(inv, balance) {
  const reason = await askPrompt(
    `Write off ${inv.currency} ${formatMoney(balance)} of ${inv.number} — reason (required):`,
  );
  const r = String(reason ?? '').trim();
  if (!r) {
    announce('Write off cancelled.');
    return;
  }
  if (!(await askConfirm(`Write off ${inv.number}? The balance stops counting as outstanding.`))) return;
  try {
    await api.post(`/invoices/${inv.id}/write-off`, { reason: r });
    announce(`${inv.number} written off.`);
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

/// Same resolution order as the server (#116): customer terms → org template
/// terms → legacy net-14 from the period end. Only the confirm text; the
/// server always owns the persisted value.
async function dueDatePreview(inv) {
  try {
    const c = state.customers.find((x) => x.id === inv.customer_id);
    let terms = c && c.payment_terms ? c.payment_terms : null;
    if (!terms) {
      const t = await api.get('/admin/invoice-template');
      terms = (t.template || {}).payment_terms || null;
    }
    const fixed = { upon_receipt: 0, net_15: 15, net_20: 20, net_30: 30, net_45: 45 };
    const d = new Date();
    if (terms) {
      const days = terms.kind === 'custom' ? Number(terms.days) : fixed[terms.kind];
      if (!Number.isFinite(days)) return null;
      d.setTime(d.getTime() + days * 86400000);
      return { date: d.toISOString().slice(0, 10), label: terms.kind };
    }
    d.setTime(new Date(`${inv.period_to}T00:00:00Z`).getTime() + 14 * 86400000);
    return { date: d.toISOString().slice(0, 10), label: 'legacy net-14' };
  } catch {
    return null; // terms are a hint only; never block the action
  }
}

async function issueInvoice(inv) {
  const preview = await dueDatePreview(inv);
  const due = preview
    ? ` It will be due ${preview.date} (${preview.label}).`
    : '';
  if (!(await askConfirm(`Issue this invoice?${due} Its entries will be locked from editing.`))) return;
  const id = inv.id;
  try {
    await api.post(`/invoices/${id}/issue`);
    invalidateLockCache(); // issuing locks its entries
    announce('Invoice issued.');
    await refreshInvoices();
  } catch (err) {
    announce(err.message);
  }
}

/// One phrasing for a delivered/queued send (#130): "disabled" means the
/// transport is a logged no-op — the UI must not claim the customer got it.
function emailOutcome(res, base) {
  return res.transport === 'smtp'
    ? base
    : `NOT sent (SMTP not configured) — would go to ${res.sent_to}. Set smtp.host in Settings.`;
}

async function emailInvoice(id) {
  try {
    const res = await api.post(`/invoices/${id}/email`);
    announce(emailOutcome(res, `Invoice emailed to ${res.sent_to} (PDF attached).`));
  } catch (err) {
    announce(`Email failed: ${err.message}`);
  }
}

/// Sends a PDF copy of the invoice to any address — e.g. the accountant
/// (#112). Recipient via the inline dialog (#102); the server validates and
/// refuses drafts/invalid shapes before sending anything.
async function emailInvoiceCopy(id) {
  const to = ((await askPrompt('Send a PDF copy to this email address:')) || '').trim();
  if (!to) {
    announce('Email copy cancelled.');
    return;
  }
  try {
    const res = await api.post(`/invoices/${id}/email-copy`, { to });
    announce(emailOutcome(res, `PDF copy sent to ${res.sent_to}.`));
  } catch (err) {
    announce(`Email copy failed: ${err.message}`);
  }
}

// Download the archived invoice PDF (#113). Fetch the binary as a blob (so
// the session cookie is sent and errors are surfaced), then hand it to a
// transient <a download>. Announcement goes through aria-live via announce().
async function downloadInvoicePdf(id, number) {
  announce('Preparing PDF…');
  try {
    const blob = await api.getBlob(`/invoices/${id}/pdf`);
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `${number}.pdf`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    // Some browsers cancel a download if the URL is revoked synchronously.
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    announce(`Invoice PDF downloaded (${blob.size} bytes).`);
  } catch (err) {
    announce(`Download failed: ${err.message}`);
  }
}

// ------------------------------------------------ invoice document preview --
//
// #133: selecting an invoice row opens a live preview built from
// GET /invoices/{id}/document (#116). The returned HTML is reduced to text
// through the DOM (childNodes' textContent) — markup is NEVER injected, the
// invariant holds; the full-format document remains the Download PDF action
// (#113). Drafts have no archived PDF yet: the panel says so honestly.

let invoicePreview = null; // { id, inv, opener }

function invoiceBalance(inv) {
  const paid = (inv.payments || []).reduce((a, p) => a + p.amount_minor, 0);
  if (inv.status === 'paid' || inv.status === 'written_off') return 0;
  return Math.max((inv.total_minor || 0) - paid, 0);
}

async function openInvoicePreview(inv, opener) {
  invoicePreview = { id: inv.id, inv, opener: opener || null };
  const card = $('invoice-preview');
  card.hidden = false;
  const facts = $('ip-facts');
  facts.textContent = '';
  const dl = (label, value) => {
    facts.appendChild(el('dt', { text: label }));
    facts.appendChild(el('dd', { text: value }));
  };
  dl('Number', inv.number);
  dl('Customer', customerName(inv.customer_id));
  dl('Period', `${inv.period_from} → ${inv.period_to}`);
  dl('Status', inv.status.replace(/_/g, ' '));
  dl('Total', `${inv.currency} ${formatMoney(inv.total_minor)}`);
  const bal = invoiceBalance(inv);
  dl('Balance', bal ? `${inv.currency} ${formatMoney(bal)}` : 'Settled');
  if (inv.due_date) dl('Due', inv.due_date);
  if (inv.write_off_reason) dl('Written off', inv.write_off_reason);
  $('ip-subject').textContent = '';
  $('ip-letter').textContent = '';
  const draft = inv.status === 'draft';
  $('ip-download').hidden = draft;
  $('ip-pdf-state').hidden = !draft;
  if (draft) {
    $('ip-pdf-state').textContent =
      'Draft — issuing the invoice archives its PDF document. Nothing is locked or sent yet.';
  } else {
    try {
      const doc = await api.get(`/invoices/${inv.id}/document`);
      if (!invoicePreview || invoicePreview.id !== inv.id) return; // superseded
      $('ip-subject').textContent = doc.subject || '';
      const parsed = new DOMParser().parseFromString(doc.html || '', 'text/html');
      $('ip-letter').textContent = [...parsed.body.childNodes]
        .map((n) => (n.textContent || '').trim())
        .filter(Boolean)
        .join('\n');
    } catch {
      $('ip-letter').textContent = 'Preview unavailable — the document could not be rendered.';
    }
  }
  card.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  announce(`Previewing invoice ${inv.number}.`);
}

function closeInvoicePreview() {
  if (!invoicePreview) return;
  const { opener } = invoicePreview;
  invoicePreview = null;
  $('invoice-preview').hidden = true;
  if (opener && document.contains(opener)) opener.focus();
}

async function removeInvoice(id) {
  if (!(await askConfirm('Delete this draft invoice?'))) return;
  try {
    await api.del(`/invoices/${id}`);
    await refreshInvoices();
    announce('Invoice deleted.');
  } catch (err) {
    announce(err.message);
  }
}

// --------------------------------------------------- invoice editor (#143) --
//
// Manual Product/Service lines and draft editing. Money stays integer:
// decimal inputs are parsed to hundredths/minor units WITHOUT floats (string
// math), amounts and totals mirror the server's exact formulas. Tracked
// lines from an existing draft show read-only and round-trip verbatim —
// the #8 rate snapshots are untouchable, and saving a draft never issues,
// emails, locks or syncs.

const ed = { id: null, tracked: [], currency: '' };

/// '12.34' -> 1234 (hundredths) without ever touching a float.
function parseHundredths(str) {
  const s = String(str).trim();
  if (!s) return 0;
  const neg = s.startsWith('-');
  const body = neg ? s.slice(1) : s;
  const [i, f = ''] = body.split('.');
  const ff = (f + '00').slice(0, 2);
  const n = Number(i || 0) * 100 + Number(ff);
  if (!Number.isFinite(n) || n < 0) return NaN;
  return neg ? -n : n;
}

const edAmount = (qtyH, priceMinor) => Math.floor((qtyH * priceMinor + 50) / 100);
const edPercent = (v, hundredths) => Math.floor((v * hundredths + 5000) / 10000);

function edTotals(subtotal, taxH, discH) {
  const discount = edPercent(subtotal, discH);
  const net = subtotal - discount;
  const tax = edPercent(net, taxH);
  return { subtotal, discount, tax, total: net + tax };
}

function edLineRow(line = null) {
  const desc = el('input', { attrs: { type: 'text', maxlength: 500, placeholder: 'Description', value: line ? line.note : '' } });
  const kind = el('select', {}, [
    el('option', { attrs: { value: 'product', selected: !line || line.item_kind !== 'service' ? '' : null }, text: 'Product' }),
    el('option', { attrs: { value: 'service' }, text: 'Service' }),
  ]);
  if (line && line.item_kind === 'service') kind.value = 'service';
  const qty = el('input', {
    cls: 'num',
    attrs: { type: 'number', min: '0.01', step: '0.01', inputmode: 'decimal', style: 'width:7ch',
      value: line && line.quantity_hundredths ? (line.quantity_hundredths / 100).toFixed(2) : '1.00' },
  });
  const price = el('input', {
    cls: 'num',
    attrs: { type: 'number', min: '0', step: '0.01', inputmode: 'decimal', style: 'width:9ch',
      value: line && line.unit_price_minor != null ? (line.unit_price_minor / 100).toFixed(2) : '0.00' },
  });
  const amount = el('td', { cls: 'num' });
  const row = el('tr', {}, [
    el('td', {}, [desc]),
    el('td', {}, [kind]),
    el('td', {}, [qty]),
    el('td', {}, [price]),
    amount,
    el('td', { cls: 'actions-col' }, [
      el('button', { type: 'button', cls: 'link danger', text: 'Remove', on: { click: () => { row.remove(); edRecalc(); } } }),
    ]),
  ]);
  const read = () => {
    const q = parseHundredths(qty.value);
    const p = parseHundredths(price.value);
    return {
      description: desc.value.trim(),
      item_kind: kind.value,
      quantity_hundredths: q,
      unit_price_minor: p,
      amount_minor: Number.isFinite(q) && Number.isFinite(p) ? edAmount(q, p) : NaN,
    };
  };
  row.__read = read;
  row.__render = () => {
    const r = read();
    amount.textContent = Number.isFinite(r.amount_minor) ? formatMoney(r.amount_minor) : '—';
  };
  for (const inp of [desc, kind, qty, price]) inp.addEventListener('input', edRecalc);
  return row;
}

function edTrackedRow(line) {
  return el('tr', {}, [
    el('td', { text: line.note || (line.project_code || '') }),
    el('td', { text: 'tracked' }),
    el('td', { cls: 'num', text: line.hours ? (line.hours / 100).toFixed(2) : '1.00' }),
    el('td', { cls: 'num', text: line.rate_minor != null ? formatMoney(line.rate_minor) : formatMoney(line.amount_minor) }),
    el('td', { cls: 'num', text: formatMoney(line.amount_minor) }),
    el('td', {}),
  ]);
}

function edRecalc() {
  const tbody = $('ed-lines').querySelector('tbody');
  for (const row of tbody.querySelectorAll('tr')) row.__render?.();
  const manual = [...tbody.querySelectorAll('tr')].map((r) => r.__read?.()).filter(Boolean);
  const trackedSub = ed.tracked.reduce((a, l) => a + l.amount_minor, 0);
  const subtotal = trackedSub + manual.reduce((a, l) => a + (Number.isFinite(l.amount_minor) ? l.amount_minor : 0), 0);
  const t = edTotals(
    subtotal,
    parseHundredths($('ed-tax').value),
    parseHundredths($('ed-discount').value),
  );
  const cur = ed.currency || 'EUR';
  const dl = $('ed-totals');
  dl.textContent = '';
  for (const [label, v] of [['Subtotal', t.subtotal], ['Discount', -t.discount], ['VAT', t.tax], ['Total', t.total]]) {
    dl.appendChild(el('dt', { text: label }));
    dl.appendChild(el('dd', { cls: 'num', text: `${v < 0 ? '-' : ''}${formatMoney(Math.abs(v))} ${cur}` }));
  }
}

function edOpen(draft) {
  ed.id = draft ? draft.id : null;
  ed.tracked = draft ? (draft.lines || []).filter((l) => l.entry_id || l.expense_id) : [];
  ed.currency = draft ? draft.currency : (state.customers.find((c) => c.id === $('invoice-customer').value)?.currency || 'EUR');
  $('ed-title').textContent = draft ? `Edit draft ${draft.number}` : 'Manual invoice';
  $('ed-customer').disabled = Boolean(draft);
  if (!draft) $('ed-customer').value = $('invoice-customer').value || $('ed-customer').value;
  const tbody = $('ed-lines').querySelector('tbody');
  tbody.textContent = '';
  for (const l of ed.tracked) tbody.appendChild(edTrackedRow(l));
  const manuals = draft ? (draft.lines || []).filter((l) => !l.entry_id && !l.expense_id) : [];
  if (manuals.length === 0 && !draft) tbody.appendChild(edLineRow());
  for (const l of manuals) tbody.appendChild(edLineRow(l));
  $('ed-tax').value = draft ? (draft.tax_hundredths / 100).toFixed(2) : '0';
  $('ed-discount').value = draft ? (draft.discount_hundredths / 100).toFixed(2) : '0';
  clearFormError($('ed-error'));
  $('invoice-editor').hidden = false;
  edRecalc();
  $('invoice-editor').scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  $('ed-lines').querySelector('tbody input')?.focus();
  announce(draft ? `Editing draft ${draft.number}.` : 'Manual invoice editor open.');
}

function edClose() {
  ed.id = null;
  $('invoice-editor').hidden = true;
}

async function edSave() {
  clearFormError($('ed-error'));
  const manual = [...$('ed-lines').querySelectorAll('tbody tr')]
    .map((r) => r.__read?.())
    .filter(Boolean);
  const lines = manual.map((m) => ({
    description: m.description, item_kind: m.item_kind,
    quantity_hundredths: m.quantity_hundredths, unit_price_minor: m.unit_price_minor,
  }));
  const taxH = parseHundredths($('ed-tax').value);
  const discH = parseHundredths($('ed-discount').value);
  if (!Number.isFinite(taxH) || !Number.isFinite(discH) || lines.some((l) => !Number.isFinite(l.quantity_hundredths) && l.quantity_hundredths !== 0)) {
    showFormError($('ed-error'), { message: 'Enter valid numbers in every line.' });
    return;
  }
  try {
    if (!ed.id) {
      const created = await api.post('/invoices/manual', {
        customer_id: $('ed-customer').value,
        tax_hundredths: taxH,
        discount_hundredths: discH,
        lines,
      });
      announce(`Draft ${created.number} saved (total ${formatMoney(created.total_minor)} ${created.currency}).`);
    } else {
      const payloadLines = [
        ...ed.tracked,
        ...manual.map((m) => ({
          kind: 'fixed',
          date: new Date().toISOString().slice(0, 10),
          note: m.description,
          item_kind: m.item_kind,
          quantity_hundredths: m.quantity_hundredths,
          unit_price_minor: m.unit_price_minor,
          amount_minor: m.amount_minor,
          entry_id: null, expense_id: null, project_code: null, task_code: null,
          hours: null, rate_minor: null,
        })),
      ];
      const updated = await api.put(`/invoices/${ed.id}`, {
        lines: payloadLines, tax_hundredths: taxH, discount_hundredths: discH,
      });
      announce(`Draft ${updated.number} updated (total ${formatMoney(updated.total_minor)} ${updated.currency}).`);
    }
    edClose();
    await refreshInvoices();
  } catch (err) {
    showFormError($('ed-error'), err);
  }
}

// --------------------------------------------------- invoice wizard (#134) --
//
// Harvest-shaped staged flow over the SAME server pipeline: preview via
// POST /invoices/preview (nothing persisted), save via the existing
// POST /invoices with project selection. Draft-only: issue/email/sync stay
// separate explicit actions on the table.

const iw = { step: 1, from: '', to: '', preview: null };

function iwPresetRange(preset) {
  const today = isoDate(new Date());
  const d = new Date(`${today}T00:00:00Z`);
  const first = (y, m) => `${y}-${String(m + 1).padStart(2, '0')}-01`;
  const lastDay = (y, m) => new Date(Date.UTC(y, m + 1, 0)).getUTCDate();
  if (preset === 'this-month') {
    iw.from = first(d.getUTCFullYear(), d.getUTCMonth());
    iw.to = `${iw.from.slice(0, 7)}-${String(lastDay(d.getUTCFullYear(), d.getUTCMonth())).padStart(2, '0')}`;
  } else if (preset === 'last-month') {
    const m = d.getUTCMonth() === 0 ? 11 : d.getUTCMonth() - 1;
    const y = d.getUTCMonth() === 0 ? d.getUTCFullYear() - 1 : d.getUTCFullYear();
    iw.from = first(y, m);
    iw.to = `${iw.from.slice(0, 7)}-${String(lastDay(y, m)).padStart(2, '0')}`;
  } else if (preset === 'this-week') {
    iw.from = mondayOf(today);
    iw.to = addDays(iw.from, 6);
  } else {
    return; // custom: inputs keep whatever the user typed
  }
  $('iw-from').value = iw.from;
  $('iw-to').value = iw.to;
}

async function iwLoadProjects() {
  iw.from = $('iw-from').value;
  iw.to = $('iw-to').value;
  const box = $('iw-projects');
  box.textContent = '';
  const cid = $('iw-customer').value;
  if (!cid || !iw.from || !iw.to || iw.from > iw.to) {
    box.appendChild(el('span', { cls: 'hint', text: 'Pick a valid period first.' }));
    return;
  }
  const [cust, entries, invoices] = await Promise.all([
    api.get(`/customers/${cid}`),
    api.get(`/entries?from=${iw.from}&to=${iw.to}`),
    api.get('/invoices'),
  ]);
  const billed = new Set();
  for (const inv of invoices.invoices || []) {
    if (inv.status !== 'draft') for (const l of inv.lines || []) if (l.entry_id) billed.add(l.entry_id);
  }
  const unbilledBy = new Map(); // project -> hundredths
  for (const e of entries.entries || []) {
    if (e.customer_id !== cid || !e.billable || billed.has(e.id) || !e.project_code) continue;
    const k = e.project_code;
    unbilledBy.set(k, (unbilledBy.get(k) || 0) + Math.round(Number(e.hours) * 100));
  }
  const projects = (await api.get(`/customers/${cid}/projects`)).projects || [];
  let shown = 0;
  for (const p of projects) {
    if (!p.active) continue;
    const hours = unbilledBy.get(p.code);
    if (!hours) continue;
    shown += 1;
    const cb = el('input', { attrs: { type: 'checkbox', id: `iw-p-${p.code}`, value: p.code } });
    box.appendChild(
      el('div', { cls: 'field checkbox' }, [
        el('label', { attrs: { for: cb.id } }, [
          cb,
          el('span', { text: `${p.code} — ${p.name} · ${(hours / 100).toFixed(2)} h unbilled` }),
        ]),
      ]),
    );
  }
  if (!shown) box.appendChild(el('span', { cls: 'hint', text: 'No uninvoiced billable work in this period.' }));
  const rate = customerName(cid) ? ` (customer rate ${formatMoney((cust.default_rate_minor ?? 0) / 100 * 100)} default)` : '';
  void rate;
}

function iwShow() {
  for (const n of [1, 2, 3]) $(`iw-step${n}`).hidden = n !== iw.step;
  const titles = ['customer and source', 'period and projects', 'review and save'];
  $('iw-title').textContent = `Create from tracked work — ${iw.step} of 3: ${titles[iw.step - 1]}`;
  $('iw-back').hidden = iw.step === 1;
  $('iw-next').hidden = iw.step === 3;
  $('iw-save').hidden = iw.step !== 3;
  clearFormError($('iw-error'));
}

function iwSelectedProjects() {
  return [...$('iw-projects').querySelectorAll('input:checked')].map((c) => c.value);
}

async function iwNext() {
  if (iw.step === 1) {
    if (!$('iw-customer').value) {
      showFormError($('iw-error'), { message: 'Pick a customer.' });
      return;
    }
    iw.step = 2;
    iwShow();
    await iwLoadProjects();
    return;
  }
  if (iw.step === 2) {
    const picks = iwSelectedProjects();
    if (!$('iw-from').value || !$('iw-to').value) {
      showFormError($('iw-error'), { message: 'Enter the period.' });
      return;
    }
    iw.from = $('iw-from').value;
    iw.to = $('iw-to').value;
    try {
      iw.preview = await api.post('/invoices/preview', {
        customer_id: $('iw-customer').value,
        from: iw.from,
        to: iw.to,
        include_expenses: $('iw-expenses').checked,
        project_codes: picks,
      });
      iwRenderReview();
      iw.step = 3;
      iwShow();
    } catch (err) {
      showFormError($('iw-error'), err);
    }
  }
}

function iwRenderReview() {
  const d = iw.preview;
  if (!d) return;
  const grouping = $('iw-grouping').value;
  const dl = $('iw-summary');
  dl.textContent = '';
  const sum = (label, v) => {
    dl.appendChild(el('dt', { text: label }));
    dl.appendChild(el('dd', { text: v }));
  };
  sum('Customer', customerName(d.customer_id));
  sum('Period', `${d.period_from} → ${d.period_to}`);
  sum('Currency', d.currency);
  sum('Lines', String((d.lines || []).length));
  sum('Total', `${formatMoney(d.total_minor)} ${d.currency}`);
  const ul = $('iw-lines');
  ul.textContent = '';
  const groups = new Map();
  for (const l of d.lines || []) {
    const key = grouping === 'project'
      ? (l.project_code || 'general')
      : grouping === 'task'
        ? `${l.project_code || 'general'}${l.task_code ? ` / ${l.task_code}` : ''}`
        : `${l.date} ${l.project_code || ''} ${l.note || l.kind}`;
    if (!groups.has(key)) groups.set(key, { h: 0, amount: 0, n: 0 });
    const g = groups.get(key);
    g.h += l.hours || 0;
    g.amount += l.amount_minor;
    g.n += 1;
  }
  for (const [key, g] of groups) {
    const detail = grouping === 'detailed'
      ? `${g.n} line(s)`
      : `${(g.h / 100).toFixed(2)} h · ${g.n} line(s)`;
    ul.appendChild(el('li', { text: `${key}: ${detail} — ${formatMoney(g.amount)} ${d.currency}` }));
  }
}

async function iwSave() {
  const picks = iwSelectedProjects();
  try {
    const created = await api.post('/invoices', {
      customer_id: $('iw-customer').value,
      from: iw.from,
      to: iw.to,
      include_expenses: $('iw-expenses').checked,
      project_codes: picks,
    });
    announce(`Draft ${created.number} created from ${created.lines.length} line(s) — review, then Issue to lock and document it.`);
    iw.step = 1;
    iw.preview = null;
    iwShow();
    await refreshInvoices();
  } catch (err) {
    showFormError($('iw-error'), err);
  }
}

function iwInit() {
  $('iw-back').addEventListener('click', () => {
    iw.step = Math.max(1, iw.step - 1);
    iwShow();
    if (iw.step === 2) iwLoadProjects().catch?.(() => {});
  });
  $('iw-next').addEventListener('click', () => iwNext().catch?.(() => {}));
  $('iw-save').addEventListener('click', iwSave);
  $('iw-preset').addEventListener('change', () => {
    iwPresetRange($('iw-preset').value);
    iwLoadProjects().catch?.(() => {});
  });
  $('iw-from').addEventListener('change', () => iwLoadProjects().catch?.(() => {}));
  $('iw-to').addEventListener('change', () => iwLoadProjects().catch?.(() => {}));
  $('iw-grouping').addEventListener('change', iwRenderReview);
  iwPresetRange('this-month');
  iwShow();
}

// ------------------------------------------------- recurring schedules ----
//
// #136: management UI for the Phase-4 recurring engine (#26). The backend
// already generates drafts for ACTIVE schedules; this surface lists them and
// pauses/resumes/deletes without touching the billing cursor
// (last_period_end), so a resumed schedule continues where it stopped.

async function refreshSchedules() {
  let schedules = [];
  try {
    schedules = (await api.get('/schedules')).schedules || [];
  } catch {
    return; // members cannot see this admin surface
  }
  const tbody = $('rec-table').querySelector('tbody');
  tbody.textContent = '';
  const action = (cls, text, fn, title) =>
    el('button', { cls, type: 'button', text, attrs: title ? { title } : {}, on: { click: fn } });
  for (const scd of schedules) {
    const active = scd.active !== false;
    tbody.appendChild(
      el('tr', {}, [
        el('th', { attrs: { scope: 'row' }, text: customerName(scd.customer_id) }),
        el('td', { text: scd.cadence }),
        el('td', { text: scd.mode === 'retainer' ? 'Fixed retainer' : 'Time + expenses' }),
        el('td', {
          cls: 'num',
          text: scd.mode === 'retainer' ? `${scd.currency} ${formatMoney(scd.retainer_amount_minor)}` : '\u2014',
        }),
        el('td', { text: scd.last_period_end || 'never' }),
        el('td', {}, [
          el('span', { cls: active ? 'badge on' : 'badge', text: active ? 'active' : 'paused' }),
        ]),
        el('td', { cls: 'actions-col' }, [
          action('link', active ? 'Pause' : 'Resume', () => toggleSchedule(scd),
            active ? 'Pause generation (drafts stop until resumed)' : 'Resume generation'),
          action('danger', 'Delete', () => removeSchedule(scd), 'Delete the schedule'),
        ]),
      ]),
    );
  }
  $('rec-table').hidden = schedules.length === 0;
  $('rec-empty').hidden = schedules.length !== 0;
}

async function addRecurring(evt) {
  evt.preventDefault();
  clearFormError($('rec-error'));
  const cid = $('rec-customer').value;
  const c = state.customers.find((x) => x.id === cid);
  const body = {
    customer_id: cid,
    cadence: $('rec-cadence').value,
    mode: $('rec-mode').value,
    retainer_amount_minor: Math.round(Number($('rec-amount').value || 0) * 100),
    currency: c ? c.currency : 'EUR',
    active: true,
  };
  try {
    await api.post('/schedules', body);
    announce('Recurring schedule added.');
    $('rec-amount').value = '0';
    await refreshSchedules();
  } catch (err) {
    showFormError($('rec-error'), err);
  }
}

async function toggleSchedule(scd) {
  try {
    const next = await api.put(`/schedules/${scd.id}`, { active: scd.active === false });
    announce(next.active ? 'Schedule resumed.' : 'Schedule paused.');
    await refreshSchedules();
  } catch (err) {
    announce(`Schedule change failed: ${err.message}`);
  }
}

async function removeSchedule(scd) {
  if (!(await askConfirm(`Delete the ${scd.cadence} schedule for ${customerName(scd.customer_id)}? Already-generated drafts are unaffected.`))) return;
  try {
    await api.del(`/schedules/${scd.id}`);
    announce('Schedule deleted.');
    await refreshSchedules();
  } catch (err) {
    announce(`Delete failed: ${err.message}`);
  }
}

// ---------------------------------------------------------------- expenses --

async function refreshCategories() {
  const data = await api.get('/categories');
  state.categories = data.categories || [];
  const ul = $('category-list');
  ul.textContent = '';
  for (const cat of state.categories) {
    ul.appendChild(
      el(
        'li',
        { cls: 'chip', text: cat.name },
        [
          el('button', {
            type: 'button',
            cls: 'chip-x',
            text: '×',
            attrs: { 'aria-label': `Delete category ${cat.name}` },
            on: {
              click: async () => {
                try {
                  await api.del(`/categories/${cat.id}`);
                  await refreshCategories();
                  await fillCategorySelect();
                } catch (err) {
                  announce(err.message);
                }
              },
            },
          }),
        ],
      ),
    );
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
  fillSelect($('expense-category'), [
    { value: '', text: 'None' },
    ...(state.categories || []).map((cat) => ({ value: cat.id, text: cat.name })),
  ]);
}

async function refreshExpenses() {
  const data = await api.get('/expenses');
  const expenses = data.expenses || [];
  const tbody = $('expense-table').querySelector('tbody');
  tbody.textContent = '';
  for (const x of expenses) {
    const c = (state.categories || []).find((k) => k.id === x.category_id);
    tbody.appendChild(
      el('tr', {}, [
        el('th', { attrs: { scope: 'row' }, text: x.date }),
        el('td', { text: customerName(x.customer_id) }),
        el('td', { text: x.project_code || '' }),
        el('td', { text: c ? c.name : '' }),
        el('td', { cls: 'num', text: `${x.currency} ${formatMoney(x.amount_minor)}` }),
        el('td', { text: x.billable ? 'yes' : 'no' }),
        el('td', { cls: 'actions-col' }, [
          el('button', {
            cls: 'danger',
            type: 'button',
            text: 'Delete',
            on: {
              click: async () => {
                if (!(await askConfirm('Delete this expense?'))) return;
                await api.del(`/expenses/${x.id}`);
                await refreshExpenses();
                announce('Expense deleted.');
              },
            },
          }),
        ]),
      ]),
    );
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
    const actions = [];
    if (s.state === 'submitted') {
      actions.push(
        el('button', {
          cls: 'link',
          type: 'button',
          text: 'Approve',
          on: { click: () => decideSubmission(s.id, 'approve') },
        }),
        el('button', {
          cls: 'danger',
          type: 'button',
          text: 'Reject',
          on: { click: () => decideSubmission(s.id, 'reject') },
        }),
      );
    }
    tbody.appendChild(
      el('tr', {}, [
        el('th', { attrs: { scope: 'row' }, text: `${s.week_start} → ${s.week_end}` }),
        el('td', { cls: 'num', text: String(s.entry_ids.length) }),
        el('td', {}, [
          el('span', { cls: s.state === 'approved' ? 'badge on' : 'badge', text: s.state }),
        ]),
        el('td', { text: s.comment || '' }),
        el('td', { cls: 'actions-col' }, actions),
      ]),
    );
  }
  $('submission-empty').hidden = subs.length !== 0;
}

async function submitWeek(evt) {
  evt.preventDefault();
  clearFormError($('submission-error'));
  try {
    await api.post('/submissions', { week_start: $('submission-week').value });
    invalidateLockCache(); // the submitted week now locks its entries
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
    invalidateLockCache(); // approve keeps the lock, reject releases it
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
      tbody.appendChild(
        el('tr', {}, [
          el('th', { attrs: { scope: 'row' }, text: s.key }),
          el('td', { text: s.hint }),
          el('td', { cls: 'actions-col' }, [
            el('button', {
              cls: 'danger',
              type: 'button',
              text: 'Delete',
              on: { click: () => deleteSecret(s.key) },
            }),
          ]),
        ]),
      );
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
  await refreshConfig();
  await refreshOrgProfile();
  await refreshInvoiceTemplate();
}

/// Effective runtime configuration card (#94). Admin surface; members get a
/// silent hide. Values resolve env > config.json > default; applying an edit
/// persists it to config.json on the data volume so restarts keep it.
let configRows = [];

function configValueForInput(v) {
  if (v === null || v === undefined) return '';
  if (typeof v === 'boolean') return String(v);
  if (typeof v === 'number') return String(v);
  return v;
}

async function refreshConfig() {
  let data;
  try {
    data = await api.get('/admin/config');
    // #130: be honest about delivery. A 200 from the email endpoints with
    // transport "disabled" means logged, not sent.
    const es = $('email-status');
    if (es) {
      es.textContent = data.email_transport === 'smtp'
        ? 'Email delivery: SMTP configured — invoices are actually sent.'
        : 'Email delivery: NOT configured — messages are logged only. Set smtp.host in credentials above or TUCANO_SMTP_HOST.';
    }
  } catch {
    $('config-table').hidden = true;
    $('config-form').hidden = true;
    return;
  }
  configRows = data.config || [];
  const tbody = $('config-table').querySelector('tbody');
  tbody.textContent = '';
  tbody.append(
    ...configRows.map((row) =>
      el('tr', {}, [
        el('th', { attrs: { scope: 'row', title: row.description }, text: row.key }),
        el('td', { cls: 'num', text: configValueForInput(row.value) }),
        el('td', {}, [
          el('span', { cls: row.source === 'default' ? 'badge' : 'badge on', text: row.source }),
        ]),
      ]),
    ),
  );
  const sel = $('config-key');
  fillSelect(sel, configRows.map((row) => ({ value: row.key, text: row.key })));
  // Pre-fill the value box when the key changes.
  sel.onchange = () => {
    const row = configRows.find((c) => c.key === sel.value);
    $('config-value').value = row ? configValueForInput(row.value) : '';
  };
  sel.onchange();
}

async function saveConfig(evt) {
  evt.preventDefault();
  clearFormError($('config-error'));
  const key = $('config-key').value;
  const raw = $('config-value').value;
  const row = configRows.find((c) => c.key === key) || {};
  let value = raw;
  if (typeof row.value === 'number') {
    if (!/^-?\d+$/.test(raw)) {
      showFormError($('config-error'), { message: 'Enter a whole number.' });
      return;
    }
    value = Number(raw);
  } else if (typeof row.value === 'boolean') {
    if (raw !== 'true' && raw !== 'false') {
      showFormError($('config-error'), { message: 'Enter true or false.' });
      return;
    }
    value = raw === 'true';
  }
  try {
    await api.request('PUT', '/admin/config', { [key]: value });
    announce(`Saved ${key}.`);
    await refreshConfig();
  } catch (err) {
    showFormError($('config-error'), err);
  }
}

// ------------------------------------------------- invoice template (#116) --

let templateVarsLoaded = false;

// ------------------------------------------------- company identity (#138) --
async function refreshOrgProfile() {
  try {
    const o = await api.get('/admin/org');
    $('org-name').value = o.name || '';
    $('org-legal').value = o.legal_id || '';
    const a = o.address || {};
    $('org-street').value = a.street || '';
    $('org-city').value = a.city || '';
    $('org-postal').value = a.postal_code || '';
    $('org-country').value = a.country || '';
  } catch { /* members cannot manage the org profile */ }
}

async function saveOrgProfile(evt) {
  evt.preventDefault();
  clearFormError($('org-error'));
  const street = $('org-street').value.trim();
  const city = $('org-city').value.trim();
  const postal = $('org-postal').value.trim();
  const country = $('org-country').value.trim();
  try {
    await api.put('/admin/org', {
      name: $('org-name').value.trim(),
      legal_id: $('org-legal').value.trim(),
      address: street || city || postal || country
        ? { street, city, postal_code: postal, country }
        : null,
    });
    announce('Company information saved.');
    await refreshOrgProfile();
  } catch (err) {
    showFormError($('org-error'), err);
  }
}

async function refreshInvoiceTemplate() {
  try {
    const data = await api.get('/admin/invoice-template');
    const t = data.template || {};
    $('template-subject').value = t.subject || '';
    $('template-body').value = t.body || '';
    $('template-footer').value = t.footer || '';
    const terms = t.payment_terms || null;
    $('template-terms').value = terms ? terms.kind : '';
    $('template-terms-days').value = terms && terms.days != null ? terms.days : '';
    $('template-terms-days-field').hidden = !terms || terms.kind !== 'custom';
    // Variable cheat-sheet rendered from the server table (same source the
    // validator enforces).
    const tbody = $('template-vars').querySelector('tbody');
    tbody.textContent = '';
    for (const v of data.variables || []) {
      tbody.appendChild(
        el('tr', {}, [
          el('th', { attrs: { scope: 'row' }, text: v.name }),
          el('td', { text: v.example }),
        ]),
      );
    }
    templateVarsLoaded = true;
    await refreshTemplatePreview();
  } catch {
    /* members cannot see the template; tab stays hidden for them */
  }
}

async function refreshTemplatePreview() {
  const card = $('template-preview');
  try {
    const data = await api.get('/invoices');
    const invoices = (data.invoices || []).filter((i) => i.status !== 'draft');
    if (invoices.length === 0) {
      card.hidden = true;
      return;
    }
    const newest = invoices[invoices.length - 1];
    const doc = await api.get(`/invoices/${newest.id}/document`);
    $('template-preview-subject').textContent = doc.subject || '';
    // The server-returned HTML is reduced to plain text through the DOM (no
    // markup is ever inserted into this page: the textContent rule holds).
    const parsed = new DOMParser().parseFromString(doc.html || '', 'text/html');
    $('template-preview-body').textContent = parsed.body.textContent;
    card.hidden = false;
  } catch {
    card.hidden = true;
  }
}

async function saveInvoiceTemplate(evt) {
  evt.preventDefault();
  clearFormError($('template-error'));
  const kind = $('template-terms').value;
  let payment_terms = null;
  if (kind) {
    payment_terms = { kind };
    if (kind === 'custom') payment_terms.days = Number($('template-terms-days').value);
  }
  try {
    await api.put('/admin/invoice-template', {
      subject: $('template-subject').value.trim(),
      body: $('template-body').value,
      footer: $('template-footer').value,
      payment_terms,
    });
    announce('Invoice template saved.');
    await refreshInvoiceTemplate();
  } catch (err) {
    showFormError($('template-error'), err);
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
  if (!(await askConfirm(`Delete secret "${key}"?`))) return;
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

// ---------------------------------------------------------------- calendar --

async function refreshCalendar(date) {
  let events = [];
  try {
    const data = await api.get(`/calendar/events?from=${date}&to=${date}`);
    events = data.events || [];
    $('calendar-section').hidden = false;
  } catch {
    $('calendar-section').hidden = true;
    return;
  }
  const ul = $('calendar-list');
  ul.textContent = '';
  for (const ev of events) {
    const start = new Date(ev.start).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    ul.appendChild(
      el('li', { cls: 'cal-event' }, [
        el('span', { text: `${start} — ${ev.title}` }),
        el('button', {
          type: 'button',
          cls: 'link',
          text: 'Log time',
          on: {
            click: () => {
              showEntryForm(true);
              $('entry-date').value = date;
              $('entry-hours').value = '';
              $('entry-note').value = ev.title;
              $('entry-hours').focus();
              $('entry-form').scrollIntoView({ behavior: 'smooth', block: 'center' });
            },
          },
        }),
      ]),
    );
  }
  $('calendar-section').hidden = events.length === 0;
}

// ------------------------------------------------------------ notifications --

async function refreshNotifications() {
  try {
    const data = await api.get('/notifications');
    const list = data.notifications || [];
    const unread = list.filter((n) => !n.read);
    $('notif-section').hidden = unread.length === 0;
    const ul = $('notif-list');
    ul.textContent = '';
    for (const n of unread) {
      ul.appendChild(
        el('li', { cls: 'notif' }, [
          el('strong', { text: n.title }),
          el('span', { text: ` ${n.body}` }),
        ]),
      );
    }
  } catch {
    $('notif-section').hidden = true;
  }
}

async function markNotificationsRead() {
  try {
    await api.post('/notifications/read');
    await refreshNotifications();
  } catch (err) {
    announce(err.message);
  }
}

// ---------------------------------------------------------------- boot ---

let suppressPanelRefresh = false; // C7: avoid double-fetch when jumping panels

function switchTab(tabId) {
  const tab = $(tabId);
  if (!tab) return;
  suppressPanelRefresh = true;
  tab.click();
  suppressPanelRefresh = false;
}

/// Reloads a panel's data when its tab is shown (keeps views fresh after
/// changes made from another panel or the API).
function refreshPanel(tabId) {
  if (suppressPanelRefresh) return; // caller will refresh explicitly
  // C7: customers & settings were missing — re-shown panels re-pull now.
  const fn = {
    'tab-timesheet': refreshTimesheetView,
    'tab-invoices': async () => {
      await refreshInvoices();
      await refreshSchedules();
    },
    'tab-expenses': refreshExpenses,
    'tab-submissions': refreshSubmissions,
    'tab-customers': async () => {
      await loadCustomers();
      await refreshCustomerTable();
      await refreshCustomerPickers();
    },
    'tab-settings': refreshSettings,
  }[tabId];
  if (fn) fn();
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
  initSegTabs(); // Day | Week inside the Timesheets section (#107)
  initWizard(); // first-run setup wizard (#111)
  // #142: rail shortcuts. Invoice selects the existing tab; Timer navigates
  // to the timesheet and focuses the timer — it NEVER presses Start.
  $('shortcut-invoices').addEventListener('click', () => $('tab-invoices').click());
  $('shortcut-timer').addEventListener('click', () => {
    $('tab-timesheet').click();
    $('timer-customer').focus();
    announce('Timer ready — choose customer and project, then press Start.');
  });

  $('day-add').addEventListener('click', () => {
    resetEntryForm();
    showEntryForm(true);
    $('entry-customer').focus();
  });
  $('day-prev').addEventListener('click', () => navigateDay(-1));
  $('day-next').addEventListener('click', () => navigateDay(1));
  $('day-today').addEventListener('click', () => selectDay(isoDate(new Date())));
  $('day-add-bottom').addEventListener('click', () => $('day-add').click());
  // Copy-from-N-days dropdown (#108): choosing an offset runs the copy.
  fillSelect(
    $('copy-days'),
    [1, 2, 3, 4, 5, 6, 7].map((n) => ({
      value: String(n),
      text: `Copy from ${n === 1 ? '1 day' : `${n} days`} ago (projects only)`,
      selected: n === 3,
    })),
  );
  $('copy-days').addEventListener('change', () =>
    copyPreviousDay(Number($('copy-days').value) || 1),
  );
  // #140: calendar navigation.
  $('cal-prev').addEventListener('click', () => {
    calState.month = shiftMonth(calState.month || $('day-date').value.slice(0, 7), -1);
    refreshTimeCal().catch?.(() => {});
  });
  $('cal-next').addEventListener('click', () => {
    calState.month = shiftMonth(calState.month || $('day-date').value.slice(0, 7), 1);
    refreshTimeCal().catch?.(() => {});
  });
  $('cal-today').addEventListener('click', () => {
    calState.month = isoDate(new Date()).slice(0, 7);
    refreshTimeCal().catch?.(() => {});
  });
  $('entry-form').addEventListener('submit', saveEntry);
  $('entry-cancel').addEventListener('click', () => {
    resetEntryForm();
    showEntryForm(false);
    announce('Entry cancelled — nothing was saved.');
  });
  $('entry-dialog').addEventListener('cancel', (e) => {
    // native Escape on real browsers: never close mid-save, cancel otherwise
    if (entrySaving) e.preventDefault();
    else {
      e.preventDefault();
      resetEntryForm();
      showEntryForm(false);
      announce('Entry cancelled — nothing was saved.');
    }
  });
  $('entry-dialog').addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !entrySaving) {
      // jsdom never fires 'cancel'; emulate it here (browsers do both).
      resetEntryForm();
      showEntryForm(false);
      announce('Entry cancelled — nothing was saved.');
    }
  });
  $('entry-customer').addEventListener('change', async (e) => {
    await fillProjectSelect($('entry-project'), e.target.value, null);
    fillTaskSelect($('entry-task'), '', null, null);
    $('entry-project').focus();
  });
  $('entry-project').addEventListener('change', async (e) => {
    await fillTaskSelect($('entry-task'), $('entry-customer').value, e.target.value, null);
  });

  // Week grid (#13): inline cell editing via delegated events.
  const wt = $('week-table');
  wt.addEventListener('focusout', (e) => {
    if (e.target.classList && e.target.classList.contains('cell-input')) commitCell(e.target);
  });
  wt.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && e.target.classList && e.target.classList.contains('cell-input')) {
      e.preventDefault();
      commitCell(e.target);
    }
  });
  $('week-date').addEventListener('change', () => refreshWeek());
  $('week-prev').addEventListener('click', () => navigateWeek(-1));
  $('week-next').addEventListener('click', () => navigateWeek(1));
  $('week-today').addEventListener('click', () => {
    $('week-date').value = isoDate(new Date());
    refreshWeek();
  });
  // Copy-from-N-weeks dropdown (#109): choosing an offset runs the copy.
  fillSelect(
    $('week-copy-weeks'),
    [1, 2, 3, 4].map((n) => ({
      value: String(n),
      text: n === 1 ? 'Copy from last week (projects only)' : `Copy from ${n} weeks ago (projects only)`,
      selected: n === 1,
    })),
  );
  $('week-copy-weeks').addEventListener('change', () =>
    copyLastWeek(Number($('week-copy-weeks').value) || 1),
  );
  $('week-track').addEventListener('click', async () => {
    const today = isoDate(new Date());
    const days = weekState.days.length === 7 ? weekState.days : [today];
    $('day-date').value = days.includes(today) ? today : days[0];
    showTimesheet('ts-day');
    await refreshDay();
    $('day-add').click();
  });
  $('week-add-row').addEventListener('click', async () => {
    await fillWeekProjectSelect();
    $('week-add-task').textContent = '';
    toggleWeekAddRow(true);
  });
  $('week-add-cancel').addEventListener('click', () => toggleWeekAddRow(false));
  $('week-add-project').addEventListener('change', fillWeekTaskSelect);
  $('week-add-confirm').addEventListener('click', async () => {
    const [cid, code] = $('week-add-project').value.split('|');
    const task = $('week-add-task').value || '';
    if (!cid || !code) {
      // Exhausted picker (or nothing chosen): explain, keep the grid as-is.
      announce('All active projects are already in this week.');
      toggleWeekAddRow(false);
      $('week-add-row').focus();
      return;
    }
    const added = addWeekRow(cid, code, task, $('week-date').value);
    toggleWeekAddRow(false);
    await refreshWeek();
    // #127: the announcement tells the truth about what happened.
    const label = code + (task ? ` / ${task}` : '');
    announce(added
      ? `Row added for ${label} — type hours to save.`
      : `${label} is already in this week — no row added.`);
    $('week-add-row').focus();
  });

  $('ip-close').addEventListener('click', closeInvoicePreview);
  $('ip-download').addEventListener('click', () => {
    if (invoicePreview) downloadInvoicePdf(invoicePreview.inv.id, invoicePreview.inv.number);
  });
  $('invoice-preview').addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeInvoicePreview();
  });
  $('org-form').addEventListener('submit', saveOrgProfile);
  $('manual-new').addEventListener('click', () => edOpen(null));
  iwInit(); // #134 staged invoice wizard
  $('ed-close').addEventListener('click', edClose);
  $('ed-add').addEventListener('click', () => {
    const row = edLineRow();
    $('ed-lines').querySelector('tbody').appendChild(row);
    row.querySelector('input').focus();
    edRecalc();
  });
  $('ed-save').addEventListener('click', edSave);
  $('ed-tax').addEventListener('input', edRecalc);
  $('ed-discount').addEventListener('input', edRecalc);
  $('rec-form').addEventListener('submit', addRecurring);
  $('contact-add').addEventListener('click', () => {
    if ($('contact-rows').children.length >= 10) {
      announce('At most 10 contacts per customer.');
      return;
    }
    $('contact-rows').appendChild(contactRow()).focus?.();
    $('contact-rows').lastChild.querySelector('input')?.focus();
  });
  $('template-form').addEventListener('submit', saveInvoiceTemplate);
  $('template-terms').addEventListener('change', () => {
    $('template-terms-days-field').hidden = $('template-terms').value !== 'custom';
  });
  $('customer-form').addEventListener('submit', saveCustomer);
  $('customer-cancel').addEventListener('click', cancelCustomerEdit);
  // #116: the day input only makes sense for `custom` terms.
  $('customer-terms').addEventListener('change', () => {
    $('customer-terms-days-field').hidden = $('customer-terms').value !== 'custom';
  });
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
  $('config-form').addEventListener('submit', saveConfig);
  $('timer-start').addEventListener('click', startTimer);
  $('notif-read').addEventListener('click', markNotificationsRead);
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
  await refreshCustomerPickers(); // fills all four pickers, no fetches
  refreshSchedules().catch?.(() => {});
  // First-run trigger (#111): zero customers after a successful login opens
  // the wizard as a modal. Once a customer exists it never auto-opens — the
  // sidebar entry reopens it manually.
  if ((state.customers || []).length === 0) openWizard({ fresh: true });
  // D1: paint the day view first, then fetch the independent panels together
  // (~17 sequential RTTs used to gate first paint; now ~2).
  const firstPaint = refreshDay();
  $('expense-date').value = today;
  await Promise.all([
    firstPaint,
    refreshWeek(),
    refreshCustomerTable(),
    refreshInvoices(),
    refreshTimer(),
    refreshNotifications(),
    refreshCategories(),
    fillCategorySelect(),
    refreshExpenses(),
    refreshSubmissions(),
    refreshSettings(),
  ]);
  $('version').textContent = 'TucanoTime';
}

// ------------------------------------------------------------ wizard (#111) --
//
// First-run setup: welcome -> customer -> project -> done. Uses the existing
// POST /customers and POST /customers/{id}/projects routes (no backend
// surface), so every step that completes is already persisted — "set up
// later" never loses a finished customer or project. Reopening with at least
// one customer jumps straight to the project step. All data through
// textContent; step changes announce via the dialog's aria-live status.

const WIZ = { step: 0, customer: null, project: null };
const WIZ_TITLES = ['Welcome', 'Add your first customer', 'Add a project', 'All set'];

/// jsdom hosts still lack the <dialog> modal API — same fallback as #102's
/// showDialog, so the harness drives the identical button flow.
function wizardShow() {
  const dlg = $('wizard-dialog');
  if (typeof dlg.showModal === 'function') dlg.showModal();
  else dlg.setAttribute('open', '');
}

function wizardHide() {
  const dlg = $('wizard-dialog');
  if (typeof dlg.close === 'function') dlg.close();
  else dlg.removeAttribute('open');
}

function openWizard(opts = {}) {
  WIZ.customer = null;
  WIZ.project = null;
  // Fresh first login starts at the welcome; a manual reopen with existing
  // customers starts at the project step (selection is not persisted).
  WIZ.step = opts.fresh || state.customers.length === 0 ? 0 : 2;
  $('wz-project-code').value = '';
  $('wz-project-name').value = '';
  wizardShow();
  wizRender();
}

function wizRender() {
  for (let n = 0; n < 4; n++) $(`wz-step-${n}`).hidden = n !== WIZ.step;
  $('wz-title').textContent = WIZ_TITLES[WIZ.step];
  $('wz-status').textContent = WIZ.step === 3
    ? 'Setup complete.'
    : `Step ${WIZ.step + 1} of 4: ${WIZ_TITLES[WIZ.step]}`;
  $('wz-back').hidden = WIZ.step === 0 || WIZ.step === 3;
  $('wz-later').hidden = WIZ.step === 3;
  $('wz-next').textContent =
    WIZ.step === 1 ? 'Create customer'
      : WIZ.step === 2 ? 'Create project'
        : WIZ.step === 3 ? 'Start recording time' : 'Next';
  clearFormError($('wz-error'));
  if (WIZ.step === 2) wizPrepareProject();
  const focusTarget = { 0: 'wz-next', 1: 'wz-cust-name', 2: 'wz-project-code', 3: 'wz-next' }[WIZ.step];
  const f = $(focusTarget);
  if (f) f.focus();
}

function wizTargetCustomer() {
  if (WIZ.customer) return WIZ.customer;
  const cid = $('wz-project-customer').value;
  return state.customers.find((c) => c.id === cid) || null;
}

function wizPrepareProject() {
  const needsPick = !WIZ.customer;
  $('wz-pick-customer-field').hidden = !needsPick;
  if (needsPick) {
    fillSelect(
      $('wz-project-customer'),
      state.customers.map((c) => ({ value: c.id, text: c.name })),
      state.customers[0] ? state.customers[0].id : null,
    );
  }
  wizPrefillFromCustomer(); // #11 prefill rule: currency + rate from the customer
}

function wizPrefillFromCustomer() {
  const c = wizTargetCustomer();
  if (!c) return;
  $('wz-project-currency').value = c.currency;
  $('wz-project-rate').value = (c.default_rate_minor / 100).toFixed(2);
}

async function wizNext() {
  if (WIZ.step === 0) {
    WIZ.step = 1;
    return wizRender();
  }
  if (WIZ.step === 1) return wizCreateCustomer();
  if (WIZ.step === 2) return wizCreateProject();
  wizardHide();
  return wizFinish();
}

async function wizCreateCustomer() {
  const name = $('wz-cust-name').value.trim();
  if (!name) {
    $('wz-error').textContent = 'name: required, at most 120 characters';
    $('wz-error').hidden = false;
    return;
  }
  try {
    WIZ.customer = await api.post('/customers', {
      name,
      currency: $('wz-cust-currency').value.trim().toUpperCase(),
      default_rate_minor: Math.round(Number($('wz-cust-rate').value) * 100),
      active: true,
    });
    await loadCustomers();
    await refreshCustomerTable();
    await refreshCustomerPickers();
    WIZ.step = 2;
    wizRender();
  } catch (err) {
    showFormError($('wz-error'), err);
  }
}

async function wizCreateProject() {
  const c = wizTargetCustomer();
  const code = $('wz-project-code').value.trim();
  if (!c || !code) {
    $('wz-error').textContent = !c ? 'pick a customer first' : 'code: required';
    $('wz-error').hidden = false;
    return;
  }
  try {
    WIZ.project = await api.post(`/customers/${c.id}/projects`, {
      code,
      name: $('wz-project-name').value.trim(),
      currency: $('wz-project-currency').value.trim().toUpperCase(),
      rate_minor: Math.round(Number($('wz-project-rate').value) * 100),
    });
    $('wz-done-text').textContent =
      `Customer "${c.name}" and project "${WIZ.project.code}" are ready.`;
    WIZ.step = 3;
    wizRender();
  } catch (err) {
    showFormError($('wz-error'), err);
  }
}

async function wizFinish() {
  // Land on the Day view with the new pair preselected: the entry form opens
  // ready for the first time entry (#111 DoD).
  $('tab-timesheet').click();
  $('day-add').click(); // resets + shows the inline entry form
  if (WIZ.customer && WIZ.project) {
    $('entry-customer').value = WIZ.customer.id;
    await fillProjectSelect($('entry-project'), WIZ.customer.id, WIZ.project.code);
    $('entry-customer').value = WIZ.customer.id; // fillProjectSelect does not touch it; defensive
    $('entry-hours').focus();
    announce('Setup complete — record your first entry.');
  }
}

function initWizard() {
  $('wizard-open').addEventListener('click', () => openWizard());
  $('wz-next').addEventListener('click', () => wizNext());
  $('wz-back').addEventListener('click', () => {
    WIZ.step = Math.max(0, WIZ.step - 1);
    wizRender();
  });
  $('wz-later').addEventListener('click', wizardHide);
  $('wz-project-customer').addEventListener('change', wizPrefillFromCustomer);
  // Keyboard flow: Enter advances exactly like clicking Next.
  $('wizard-dialog').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && e.target.tagName !== 'BUTTON') {
      e.preventDefault();
      wizNext();
    }
  });
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
  initDialog(); // one <dialog> for confirm/prompt/alert (#102)
  $('auth-form').addEventListener('submit', onAuthSubmit);
  $('logout').addEventListener('click', logout);
  loadSsoProviders(); // advertise configured identity providers (#32)
  try {
    if (await initAuth()) await startApp();
  } catch (err) {
    announce(`Failed to start: ${err.message}`);
  }
});

/// Shows "Sign in with …" buttons for each configured SSO provider. The IdP
/// redirect/POST bindings land with the production adapters; until then the
/// buttons explain the ACS endpoint (honest Beta behaviour, nothing silently
/// fails).
async function loadSsoProviders() {
  try {
    const st = await api.get('/auth/sso/providers');
    const providers = st.providers || [];
    if (providers.length === 0) return;
    $('sso-box').hidden = false;
    const box = $('sso-buttons');
    box.textContent = '';
    for (const name of providers) {
      box.appendChild(
        el('button', {
          type: 'button',
          cls: 'secondary',
          text: `Sign in with ${name}`,
          on: {
            click: () => {
              announce(
                `Point your ${name} IdP at POST /auth/sso/assertion (provider "${name}"). ` +
                'Local sign-in stays available.',
              );
            },
          },
        }),
      );
    }
  } catch {
    /* providers are optional */
  }
}
