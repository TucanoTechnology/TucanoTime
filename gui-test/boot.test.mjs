// #188: GUI boot & async correctness. Boots the real app.js in jsdom and
// guards the fixes: el() style passthrough (chart heights), member-safe boot
// (admin-tier /invoices), dashboard customer filter population, write-off
// lock-cache invalidation, and the day-refresh stale-response guard.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { JSDOM } from 'jsdom';

const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const app = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

function makeDom(prelude = '') {
  const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
  dom.window.HTMLDialogElement.prototype.showModal = function () { this.open = true; };
  dom.window.HTMLDialogElement.prototype.close = function () { this.open = false; };
  const exports = `${app}
${prelude}
window.bootTest = {
  dashRenderOverview, refreshInvoices, refreshCustomerPickers,
  writeOffInvoice, navigateDay, weekLockIds, state,
  peekLockCache: () => weekLockCache,
  liveText: () => document.getElementById('live-region').textContent,
  api,
};`;
  dom.window.eval(exports);
  return { dom, context: dom.window.bootTest, document: dom.window.document };
}

test('the invoice chart renders real bar heights via el() style (#188)', () => {
  const { dom, context, document } = makeDom();
  const invoices = [
    {
      id: 'i1', number: 'INV-0001', status: 'issued', currency: 'EUR',
      period_to: '2026-03-10', total_minor: 180000, payments: [],
    },
    {
      id: 'i2', number: 'INV-0002', status: 'partly_paid', currency: 'EUR',
      period_to: '2026-06-20', total_minor: 90000,
      payments: [{ amount_minor: 50000, received_at: '2026-06-30T00:00:00Z' }],
    },
  ];
  context.dashRenderOverview(invoices);
  const bars = [...document.querySelectorAll('#inv-chart .inv-bar')];
  assert.equal(bars.length, 24, 'twelve months x open+paid');
  for (const bar of bars) {
    assert.match(
      bar.getAttribute('style') || '',
      /height:\s*\d+px/,
      'computed bar height must reach the DOM (#188: el() dropped style)',
    );
  }
  const tallest = Math.max(
    ...bars.map((b) => Number(/height:(\d+)px/.exec(b.getAttribute('style'))[1])),
  );
  assert.ok(tallest > 2, `non-zero data renders taller than the 2px floor (got ${tallest})`);
  dom.window.close();
});

test('a member boots the full app despite admin-tier 403s (#188)', async () => {
  // The real boot path: jsdom fires DOMContentLoaded, app.js runs
  // initAuth() -> startApp(). Admin-tier endpoints answer exactly what a
  // member sees (403); before #188 the /invoices rejection aborted startApp.
  const stub = `
api.get = async (path) => {
  if (path === '/auth/status') return { initialised: true };
  if (path === '/auth/me') return { id: 'm1', name: 'Mem', email: 'm@x.test', role: 'member' };
  if (path === '/auth/sso/providers') return { providers: [] };
  if (path === '/invoices' || path === '/invoices/summary' || path === '/users'
      || path === '/schedules' || path === '/retainers' || path.startsWith('/admin/')) {
    const err = new Error('administrator role required');
    err.status = 403;
    throw err;
  }
  return { entries: [], customers: [], projects: [], submissions: [], expenses: [],
           claims: [], notifications: [], categories: [], item_types: [], timer: null };
};
api.post = async () => ({});
`;
  const { dom, document } = makeDom(stub);
  // Poll for boot completion (the DOMContentLoaded handler is async).
  for (let i = 0; i < 100; i += 1) {
    if (document.getElementById('version').textContent) break;
    await new Promise((done) => setTimeout(done, 10));
  }
  assert.equal(
    document.getElementById('version').textContent,
    'TucanoTime',
    'startApp must run to completion for members',
  );
  assert.equal(document.getElementById('auth-overlay').hidden, true, 'member is signed in');
  const live = document.getElementById('live-region').textContent;
  assert.ok(!live.includes('Failed to start'), `no boot error announcement (got "${live}")`);
  dom.window.close();
});

test('dashboard customer filter is populated by refreshCustomerPickers (#188)', async () => {
  const { dom, context, document } = makeDom();
  context.state.customers = [
    { id: 'c1', name: 'ACME', currency: 'EUR', active: true, default_rate_minor: 6000 },
    { id: 'c2', name: 'Globex', currency: 'USD', active: true, default_rate_minor: 9000 },
  ];
  await context.refreshCustomerPickers();
  const filter = document.getElementById('inv-f-customer');
  assert.equal(filter.options.length, 3, 'placeholder + two customers');
  assert.equal(filter.options[0].text, 'All customers');
  assert.equal(filter.options[0].value, '', 'placeholder means "no filter"');
  assert.equal(filter.options[1].value, 'c1');
  dom.window.close();
});

test('write-off drops the lock cache because it releases entry locks (#188)', async () => {
  const { dom, context } = makeDom(`
askPrompt = async () => 'customer never paid';
askConfirm = async () => true;
refreshInvoices = async () => {};
`);
  context.api.get = async (path) => {
    if (path === '/submissions') {
      return { submissions: [{ state: 'submitted', entry_ids: ['e1'] }] };
    }
    if (path === '/invoices') return { invoices: [] };
    return {};
  };
  const ids = await context.weekLockIds();
  assert.ok(ids.has('e1'), 'lock cache warm from a submitted week');
  assert.ok(context.peekLockCache().ids === ids, 'weekLockIds populated the TTL cache');
  context.api.post = async () => ({});
  await context.writeOffInvoice({ id: 'i1', number: 'INV-0001', currency: 'EUR' }, 6000);
  assert.equal(
    context.peekLockCache().ids,
    null,
    'write-off changes is_open() (src/lock.rs), so the cache must be invalidated',
  );
  dom.window.close();
});

test('rapid day navigation renders the newest response, not an older one (#188)', async () => {
  const { dom, context, document } = makeDom();
  context.api.get = (path) => {
    if (path.startsWith('/entries?date=')) {
      const date = decodeURIComponent(path.slice('/entries?date='.length));
      const body = {
        entries: [{
          id: `e-${date}`,
          date,
          hours: date === '2026-10-01' ? 1 : 5,
          customer_id: 'c',
          project_code: 'P1',
          billable: true,
          note: '',
        }],
      };
      if (date === '2026-10-01') {
        // The FIRST (older) navigation resolves LAST: without the sequence
        // guard its 1h render would clobber the 5h one.
        return new Promise((done) => setTimeout(() => done(body), 20));
      }
      return Promise.resolve(body);
    }
    return Promise.resolve({ submissions: [], invoices: [], notifications: [], entries: [] });
  };
  document.getElementById('day-date').value = '2026-09-30';
  context.navigateDay(1); // → 10-01, delayed response
  context.navigateDay(1); // → 10-02, resolves immediately
  await new Promise((done) => setTimeout(done, 60)); // let the late response land
  assert.equal(
    document.getElementById('day-total').textContent,
    '5:00',
    'a stale day response must not overwrite the newest render',
  );
  dom.window.close();
});
