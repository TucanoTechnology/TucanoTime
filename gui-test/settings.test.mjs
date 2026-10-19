// Settings restructure: seven focused sections, the Email (SMTP) section and
// the app-wide heading/card pattern. These tests drive the real index.html +
// app.js in jsdom and pin:
//   * the tab strip order, each route's panel, and member visibility;
//   * Company saving through PUT /admin/org;
//   * the Invoice template saving through PUT /admin/invoice-template, with
//     its server-rendered %variable% cheat-sheet;
//   * the Email section: GET prefill (never the password), PUT of changed
//     values only, the vault-disabled path, delivery status and test-send
//     success/422/502 with single-flight;
//   * the recipient dialog's Settings link route;
//   * the a11y contract (headings/cards, labelled fields, live regions,
//     horizontal keyboard navigation).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { JSDOM } from 'jsdom';
import { loadAppSource } from './load-app.mjs';
import { waitFor } from './wait-for.mjs';

const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const app = loadAppSource();

const EMAIL = {
    vault_enabled: true,
    configured: true,
    transport: 'smtp',
    host: 'smtp.tucanotime.test',
    port: 587,
    username: 'mailer',
    from: 'billing@tucanotime.test',
    has_password: true,
};

const ORG = {
    name: 'Tucano',
    legal_id: 'BE0123456789',
    legal_id_label: 'VAT',
    address: { street: 'Harbour 9', city: 'Antwerp', postal_code: '2000', country: 'BE' },
    from_name: 'Tucano Billing',
    reply_to: 'billing@tucanotime.test',
    accent: '#e95420',
};

const TEMPLATE = {
    template: {
        subject: 'Invoice %invoice_number%',
        body: 'Dear %customer_name%',
        footer: 'Bank: BE00',
        payment_terms: { kind: 'net_30' },
        labels: {
            description: 'Work',
            quantity: 'Hours',
            unit_price: 'Rate',
            subtotal: 'Subtotal',
            discount: 'Discount',
            tax: 'VAT',
            total: 'Due',
            from: 'Issued by',
            invoice_for: 'Billed to',
            invoice_id: 'Invoice no.',
            issue_date: 'Issued',
            due_date: 'Pay by',
            subject: 'Re',
            payment_heading: 'How to pay',
            item_type: 'Kind',
            payments: 'Settlements',
            amount_due: 'Balance due',
        },
    },
    variables: [
        { name: '%customer_name%', example: 'Tucano' },
        { name: '%invoice_number%', example: 'INV-0001' },
        { name: '%invoice_issue_month%', example: 'March' },
    ],
};

// SET-04/SET-08: a representative /admin/config payload — a numeric knob, a
// Currency knob (the only null-accepting kind) and a restart-required key.
const CONFIG = [
    { key: 'reminder_days', value: 7, source: 'default', default: 7, description: 'Days between reminders' },
    { key: 'base_currency', value: 'EUR', source: 'file', default: null, description: 'Base currency' },
    { key: 'sso_admin_group', value: '', source: 'default', default: '', description: 'SSO admin group' },
];

const AUDIT = {
    events: [
        { ts: '2026-10-18T20:00:00Z', event: 'login_ok', subject: 'admin@tucanotime.test' },
        { ts: '2026-10-18T19:59:00Z', event: 'user_created', subject: 'qa@tucanotime.test' },
    ],
};

// GET /api/admin/vault — the captured enabled/key-file state from the contract.
const VAULT_ENABLED = {
    enabled: true,
    source: 'key_file',
    disabled_reason: null,
    fingerprint: 'sha256:9f3a1c0b',
    key_file_name: 'vault.key',
    key_file_present: true,
    secrets_count: 0,
    external_override: false,
    can_enable: false,
    can_disable: true,
    can_regenerate: true,
};

const VAULT_DISABLED_CONFIG = {
    enabled: false,
    source: 'none',
    disabled_reason: 'config',
    fingerprint: null,
    key_file_name: null,
    key_file_present: false,
    secrets_count: 0,
    external_override: false,
    can_enable: true,
    can_disable: false,
    can_regenerate: false,
};

const VAULT_DISABLED_WRITE = {
    ...VAULT_DISABLED_CONFIG,
    disabled_reason: 'write_failed',
};

const VAULT_DISABLED_ENV = {
    ...VAULT_DISABLED_CONFIG,
    disabled_reason: 'env',
    can_enable: false,
};

const VAULT_ENV_OVERRIDE = {
    ...VAULT_ENABLED,
    source: 'env',
    fingerprint: 'sha256:env0override',
    key_file_name: null,
    key_file_present: false,
    external_override: true,
    can_disable: false,
    can_regenerate: false,
};

function setup({
    email = EMAIL,
    putError = null,
    postResult = { sent_to: 'qa@tucanotime.test', transport: 'smtp' },
    postError = null,
    getError = null,
    config = CONFIG,
    audit = AUDIT,
    vault = VAULT_ENABLED,
    vaultNext = {},
    vaultResults = {},
    vaultErrors = {},
    secrets = [],
} = {}) {
    const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/settings/users' });
    const { window } = dom;
    window.HTMLDialogElement.prototype.showModal = function showModal() {
        this.open = true;
        this.setAttribute('open', '');
    };
    window.HTMLDialogElement.prototype.close = function close() {
        this.open = false;
        this.removeAttribute('open');
    };
    const prelude = `
      window.calls = { get: [], put: [], post: [], request: [], preview: [] };
      window.__email = ${JSON.stringify(email)};
      window.__org = ${JSON.stringify(ORG)};
      window.__template = ${JSON.stringify(TEMPLATE)};
      window.__config = ${JSON.stringify(config)};
      window.__audit = ${JSON.stringify(audit)};
      window.__getError = ${JSON.stringify(getError)};
      window.__putError = ${JSON.stringify(putError)};
      window.__postResult = ${JSON.stringify(postResult)};
      window.__postError = ${JSON.stringify(postError)};
      window.__vault = ${JSON.stringify(vault)};
      window.__vaultNext = ${JSON.stringify(vaultNext)};
      window.__vaultResults = ${JSON.stringify(vaultResults)};
      window.__vaultErrors = ${JSON.stringify(vaultErrors)};
      window.__secrets = ${JSON.stringify(secrets)};
      // The live preview's transport. Tests replace __previewHandler to hold a
      // render open, or to answer 422/304.
      window.__previewHandler = null;
      api.get = async (path) => {
        window.calls.get.push(path);
        if (window.__getError && window.__getError[path]) {
          const raw = window.__getError[path];
          const err = new Error(raw.message);
          err.status = raw.status;
          err.payload = { error: { code: raw.code || 'error', message: raw.message } };
          throw err;
        }
        if (path === '/auth/status') return { initialised: true };
        if (path === '/auth/sso/providers') return { providers: [] };
        if (path === '/auth/me') throw new Error('no session');
        if (path === '/admin/vault') return window.__vault;
        if (path === '/admin/secrets') return { secrets: window.__secrets };
        if (path === '/admin/email') return window.__email;
        if (path === '/admin/org') return window.__org;
        if (path === '/admin/invoice-template') return window.__template;
        if (path === '/admin/config') return { config: window.__config };
        if (path === '/audit') return window.__audit;
        return {};
      };
      api.put = async (path, body) => {
        window.calls.put.push({ path, body });
        if (window.__putError) {
          const raw = window.__putError;
          const err = new Error(raw.message);
          err.status = raw.status;
          err.payload = { error: { code: raw.code || 'error', message: raw.message } };
          throw err;
        }
        if (path === '/admin/email') return { ...window.__email, ...body, configured: true };
        return body;
      };
      api.request = async (method, path, body) => {
        window.calls.request.push({ method, path, body });
        if (method === 'PUT' && path === '/admin/config') {
          for (const [k, v] of Object.entries(body || {})) {
            const row = window.__config.find((entry) => entry.key === k);
            if (row) {
              row.value = v;
              row.source = v === null ? 'default' : 'file';
            }
          }
          return { ok: true };
        }
        return {};
      };
      api.post = async (path, body) => {
        window.calls.post.push({ path, body });
        if (path.startsWith('/admin/vault')) {
          const raw = window.__vaultErrors[path];
          if (raw) {
            const err = new Error(raw.message);
            err.status = raw.status;
            err.payload = { error: { code: raw.code || 'error', message: raw.message } };
            throw err;
          }
          // A successful action moves the simulated server to the next state;
          // the GUI must re-fetch GET /admin/vault rather than trust this body.
          if (window.__vaultNext[path]) window.__vault = window.__vaultNext[path];
          if (window.__vaultResults[path]) return window.__vaultResults[path];
          return { ...window.__vault };
        }
        if (window.__postError) {
          const raw = window.__postError;
          const err = new Error(raw.message);
          err.status = raw.status;
          err.payload = { error: { code: raw.code || 'error', message: raw.message } };
          throw err;
        }
        return window.__postResult;
      };
      api.requestBlob = async (method, path, body, opts) => {
        window.calls.preview.push({ method, path, body, ifNoneMatch: (opts || {}).ifNoneMatch ?? null });
        if (window.__previewHandler) return window.__previewHandler(method, path, body, opts);
        return { notModified: false, etag: '"preview-etag-1"', blob: { type: 'application/pdf' }, status: 200 };
      };
      window.createdUrls = [];
      window.revokedUrls = [];
      // A monotonic sequence, so clearing the ledger after the opening render
      // does not renumber the next URL.
      window.urlSeq = 0;
      URL.createObjectURL = (blob) => {
        window.urlSeq += 1;
        const url = 'blob:preview-' + window.urlSeq;
        window.createdUrls.push(url);
        return url;
      };
      URL.revokeObjectURL = (url) => window.revokedUrls.push(url);
      initDialog();
      initTabs();
      initSettingsTabs();
      wireSettingsControls();
    `;
    window.eval(`${app}\n${prelude}\nwindow.t = {
      applyRoute, normaliseRoute, showAccount, initSettingsTabs,
      refreshSettings, refreshConfig, saveConfig, refreshAuditLog, resetBuiltInLabels,
      refreshOrgProfile, saveOrgProfile, refreshInvoiceTemplate, saveInvoiceTemplate,
      refreshEmailSettings, saveEmailSettings, sendTestEmail, EMAIL_SETTINGS_ROUTE,
      refreshVault, renderVault, refreshSecrets, enableVault, disableVault, regenerateVault,
      openVaultSettings, VAULT_SETTINGS_ROUTE,
      initTemplatePreview, renderTemplatePreview, scheduleTemplatePreview,
      releaseTemplatePreview, openTemplatePreviewInNewTab, templatePayload,
      TEMPLATE_PREVIEW_DEBOUNCE_MS, RESTART_REQUIRED_KEYS, NULLABLE_CONFIG_KEYS, state,
      calls: () => window.calls,
    };`);
    return { dom, document: window.document, window, t: window.t };
}

const tabs = (document) => [...document.querySelectorAll('#settings-tabs [role="tab"]')];
// The shell wraps long labels across source lines; the rendered/accessible
// text collapses whitespace, so compare that form rather than the raw markup.
const collapsedText = (element) => element.textContent.replace(/\s+/g, ' ').trim();
const visibleTabs = (document) => tabs(document).filter((tab) => !tab.hidden).map((tab) => tab.id);
const selected = (document) => tabs(document).filter((tab) => tab.getAttribute('aria-selected') === 'true').map((tab) => tab.id);
const setValue = (document, id, value) => {
    document.getElementById(id).value = value;
};

const ROUTES = [
    ['/settings/users', 'settings-tab-users', 'settings-panel-users'],
    ['/settings/company', 'settings-tab-company', 'settings-panel-company'],
    ['/settings/invoice-template', 'settings-tab-invoice-template', 'settings-panel-invoice-template'],
    ['/settings/catalog', 'settings-tab-catalog', 'settings-panel-catalog'],
    ['/settings/expenses', 'settings-tab-expenses', 'settings-panel-expenses'],
    ['/settings/email', 'settings-tab-email', 'settings-panel-email'],
    ['/settings/security', 'settings-tab-security', 'settings-panel-security'],
];

// ------------------------------------------------------------- tab order ----

test('Settings lists the seven focused tabs in order for an admin', () => {
    const { dom, document } = setup();
    assert.deepEqual(tabs(document).map((tab) => tab.id), ROUTES.map(([, tab]) => tab));
    assert.deepEqual(
        tabs(document).map((tab) => tab.textContent.trim()),
        ['Users', 'Company', 'Invoice template', 'Products & services', 'Expense categories', 'Email', 'Security & runtime'],
    );
    for (const [tab, panelId] of ROUTES.map(([, tab, panel]) => [tab, panel])) {
        const panel = document.getElementById(panelId);
        assert.ok(panel, `${panelId} exists`);
        assert.equal(panel.getAttribute('role'), 'tabpanel');
        assert.equal(panel.getAttribute('aria-labelledby'), tab);
    }
    dom.window.close();
});

test('each Settings route activates its own panel', () => {
    const { dom, document, t } = setup();
    for (const [route, tabId, panelId] of ROUTES) {
        assert.equal(t.normaliseRoute(route), route, `${route} is a known route`);
        assert.equal(t.applyRoute(route), true, `${route} applies`);
        assert.deepEqual(selected(document), [tabId], `${route} selects ${tabId}`);
        assert.equal(document.getElementById(panelId).hidden, false, `${route} shows ${panelId}`);
        for (const [, , other] of ROUTES) {
            if (other !== panelId) assert.equal(document.getElementById(other).hidden, true, `${other} stays hidden`);
        }
    }
    // /settings/invoices is gone: bookmarks fall back like any unknown path.
    assert.equal(t.normaliseRoute('/settings/invoices'), null);
    dom.window.close();
});

test('a member sees only Expense categories', () => {
    const { dom, document, window, t } = setup();
    t.showAccount({ role: 'member', name: 'Mem' });
    t.initSettingsTabs();
    assert.deepEqual(visibleTabs(document), ['settings-tab-expenses']);
    assert.deepEqual(selected(document), ['settings-tab-expenses']);
    assert.equal(document.getElementById('settings-panel-expenses').hidden, false);
    for (const [route, , panelId] of ROUTES) {
        if (route === '/settings/expenses') continue;
        assert.equal(document.getElementById(panelId).hidden, true, `${panelId} is closed for a member`);
    }
    // A member deep link into an admin section is refused, not rendered.
    assert.equal(t.applyRoute('/settings/email'), true, 'the Settings section still opens');
    assert.deepEqual(selected(document), ['settings-tab-expenses'], 'the URL cannot override the tier');
    window.close?.();
    dom.window.close();
});

// --------------------------------------------------------------- company ----

test('Company loads its identity and saves via PUT /admin/org', async () => {
    const { dom, document, t } = setup();
    await t.refreshOrgProfile();
    assert.equal(document.getElementById('org-name').value, 'Tucano');
    assert.equal(document.getElementById('org-city').value, 'Antwerp');
    assert.equal(document.getElementById('org-accent').value, '#e95420');
    assert.equal(document.getElementById('accent-swatch').style.background !== '', true, 'the swatch paints');

    setValue(document, 'org-name', 'Tucano Invoice Co');
    await t.saveOrgProfile({ preventDefault() { } });
    const call = t.calls().put.find((entry) => entry.path === '/admin/org');
    assert.deepEqual(JSON.parse(JSON.stringify(call.body)), {
        name: 'Tucano Invoice Co',
        legal_id: 'BE0123456789',
        legal_id_label: 'VAT',
        from_name: 'Tucano Billing',
        reply_to: 'billing@tucanotime.test',
        accent: '#e95420',
        address: { street: 'Harbour 9', city: 'Antwerp', postal_code: '2000', country: 'BE' },
    });
    dom.window.close();
});

test('Company pairs the legal id with its printed label and explains the From block (#354)', async () => {
    const { dom, document, t } = setup();
    await t.refreshOrgProfile();
    const legal = document.getElementById('org-legal');
    const label = document.getElementById('org-legal-label');
    assert.equal(legal.value, 'BE0123456789');
    assert.equal(label.value, 'VAT', 'the stored label loads beside the id');
    assert.equal(label.maxLength, 40, 'the label is bounded like the API field');
    assert.equal(label.placeholder, 'Tax ID', 'the placeholder names a neutral registration id');
    assert.equal(label.labels.length, 1);
    assert.match(label.labels[0].textContent, /legal \/ registration\s+id\s+label/i);
    const help = document.getElementById('org-legal-group-help');
    assert.match(help.textContent, /VAT \/ company id/, 'the default wording is explained');
    assert.match(help.textContent, /leave it empty/i);
    const note = document.getElementById('org-legal-group').querySelector('p.p-form-help-text');
    assert.match(note.textContent, /From/, 'the note points at the invoice From block');
    assert.match(note.textContent, /address lines/i);

    setValue(document, 'org-legal-label', 'Tax ID');
    await t.saveOrgProfile({ preventDefault() { } });
    const call = t.calls().put.find((entry) => entry.path === '/admin/org');
    assert.equal(call.body.legal_id_label, 'Tax ID', 'the label round-trips through PUT /admin/org');
    dom.window.close();
});

// ------------------------------------------------------- invoice template ----

test('Invoice template loads the cheat-sheet and saves labels with the template', async () => {
    const { dom, document, t } = setup();
    await t.refreshInvoiceTemplate();
    assert.equal(document.getElementById('template-subject').value, 'Invoice %invoice_number%');
    assert.equal(document.getElementById('lb-total').value, 'Due');
    // The ten document labels (#354) load from the same `labels` object, each
    // in its own input whose placeholder is the built-in default.
    assert.equal(document.getElementById('lb-from').value, 'Issued by');
    assert.equal(document.getElementById('lb-invoice-for').value, 'Billed to');
    assert.equal(document.getElementById('lb-invoice-id').value, 'Invoice no.');
    assert.equal(document.getElementById('lb-issue-date').value, 'Issued');
    assert.equal(document.getElementById('lb-due-date').value, 'Pay by');
    assert.equal(document.getElementById('lb-subject').value, 'Re');
    assert.equal(document.getElementById('lb-payment-heading').value, 'How to pay');
    assert.equal(document.getElementById('lb-item-type').value, 'Kind');
    assert.equal(document.getElementById('lb-payments').value, 'Settlements');
    assert.equal(document.getElementById('lb-amount-due').value, 'Balance due');
    const rows = [...document.querySelectorAll('#template-vars tbody tr')];
    assert.equal(rows.length, 3, 'the %variable% cheat-sheet comes from the server');
    assert.match(rows.map((row) => row.textContent).join(' '), /%invoice_issue_month%/);
    assert.equal(t.calls().preview.length, 0, 'a hidden section never renders a preview');

    setValue(document, 'lb-total', 'Amount due');
    setValue(document, 'lb-amount-due', 'Total payable');
    await t.saveInvoiceTemplate({ preventDefault() { } });
    const call = t.calls().put.find((entry) => entry.path === '/admin/invoice-template');
    assert.equal(call.body.labels.total, 'Amount due');
    assert.equal(call.body.labels.description, 'Work');
    assert.equal(call.body.labels.amount_due, 'Total payable', 'new labels travel in the same payload');
    assert.equal(call.body.payment_terms.kind, 'net_30');
    dom.window.close();
});

test('the Document labels section is grouped, optional and prints its defaults (#354)', () => {
    const { dom, document } = setup();
    const group = document.getElementById('document-labels');
    assert.ok(group, 'the ten document labels live in one group');
    assert.equal(group.querySelector('legend').textContent, 'Document labels');
    const expected = {
        'lb-from': 'From',
        'lb-invoice-for': 'Invoice for',
        'lb-invoice-id': 'Invoice ID',
        'lb-issue-date': 'Issue date',
        'lb-due-date': 'Due date',
        'lb-subject': 'Subject',
        'lb-payment-heading': 'Notes',
        'lb-item-type': 'Item type',
        'lb-payments': 'Payments',
        'lb-amount-due': 'Amount due',
    };
    for (const [id, placeholder] of Object.entries(expected)) {
        const input = document.getElementById(id);
        assert.ok(input, `${id} exists`);
        assert.equal(input.maxLength, 40, `${id} is bounded like the API field`);
        assert.equal(input.placeholder, placeholder, `${id} shows its built-in default`);
        assert.equal(input.labels.length, 1, `${id} has one label`);
        assert.equal(input.labels[0].getAttribute('for'), id);
    }
    // The original line-column overrides keep working beside the new set.
    for (const id of ['lb-description', 'lb-quantity', 'lb-unit-price', 'lb-subtotal', 'lb-discount', 'lb-tax', 'lb-total']) {
        assert.ok(document.getElementById(id), `${id} is still present`);
    }
    assert.match(document.getElementById('template-subject-help').textContent, /%period_month_padded%/);
    assert.match(document.getElementById('template-subject-help').textContent, /%period_year%/);
    assert.match(document.getElementById('template-subject-help').textContent, /dd\/mm\/yyyy/);
    assert.match(document.getElementById('template-subject-help').textContent, /EUR €1,234\.56/);
    dom.window.close();
});

// -------------------------------------------------- live template preview ---
//
// The preview card replaces the retired "newest invoice" document: it renders
// the *candidate* template against the API's sample invoice, so it works with no
// invoice in the store and reflects unsaved edits.

/// Open the section with the stored template loaded, and wait for the render the
/// visibility change starts. The ledgers are cleared after it, so each test only
/// sees the requests and URLs it caused.
async function openPreview(t, window, document) {
    await t.refreshInvoiceTemplate();
    assert.equal(t.calls().preview.length, 0, 'a hidden section renders nothing');
    t.initTemplatePreview();
    document.getElementById('settings-panel-invoice-template').hidden = false;
    await waitFor(() => t.calls().preview.length === 1, {
        message: 'the visible section to render its first preview',
    });
    await waitFor(() => document.getElementById('template-preview-frame').getAttribute('src') !== null, {
        message: 'the first preview to be framed',
    });
    t.calls().preview.length = 0;
    window.createdUrls.length = 0;
    window.revokedUrls.length = 0;
}

// #365: the smoke run caught Firefox's "Layout was forced before the page was
// fully loaded" on the view *after* the Invoice template section: the framed
// blob PDF was still live across the route change because the observer that
// releases it only runs as a microtask. Leaving the section must drop the frame
// and revoke its URL in the same tick as the tab switch.
test('leaving the Invoice template section releases the framed preview synchronously (#365)', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    const frame = document.getElementById('template-preview-frame');
    const framed = frame.getAttribute('src');
    assert.ok(framed, 'the preview is framed before the switch');

    document.getElementById('settings-tab-users').click();

    assert.equal(frame.getAttribute('src'), null, 'the frame is released in the same tick');
    assert.ok(window.revokedUrls.includes(framed), 'the blob URL is revoked in the same tick');
    const blank = document.getElementById('template-preview-frame');
    assert.notEqual(blank, frame, 'the live frame element is replaced, so its PDF viewer is torn down');
    dom.window.close();
});

test('returning to the Invoice template section re-renders the released preview (#365)', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    const framed = document.getElementById('template-preview-frame').getAttribute('src');

    document.getElementById('settings-tab-users').click();
    assert.equal(document.getElementById('template-preview-frame').getAttribute('src'), null,
        'the frame is released in the same tick');

    document.getElementById('settings-tab-invoice-template').click();
    await waitFor(() => {
        const frame = document.getElementById('template-preview-frame');
        return frame.getAttribute('src') !== null && frame.getAttribute('src') !== framed;
    }, { message: 'the re-shown section to render a fresh preview' });
    dom.window.close();
});

test('typing in the body and labels debounces one preview POST with the form payload', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    const body = document.getElementById('template-body');
    const total = document.getElementById('lb-total');
    body.value = 'Dear %customer_name%, please pay.';
    body.dispatchEvent(new window.Event('input', { bubbles: true }));
    total.value = 'Amount due';
    total.dispatchEvent(new window.Event('input', { bubbles: true }));
    total.dispatchEvent(new window.Event('change', { bubbles: true }));
    await waitFor(() => t.calls().preview.length === 1, { message: 'the debounced render' });
    await waitFor(() => document.getElementById('template-preview-status').textContent === 'Preview updated', {
        message: 'the debounced render to finish',
    });
    // One event-loop turn (not a fixed wait): any timer the burst had queued
    // would run now, so a trailing request would be counted below.
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(t.calls().preview.length, 1, 'one render for the whole burst');
    const call = t.calls().preview[0];
    assert.equal(call.method, 'POST');
    assert.equal(call.path, '/admin/invoice-template/preview');
    assert.deepEqual(JSON.parse(JSON.stringify(call.body)), {
        subject: 'Invoice %invoice_number%',
        body: 'Dear %customer_name%, please pay.',
        footer: 'Bank: BE00',
        payment_terms: { kind: 'net_30' },
        labels: {
            description: 'Work',
            quantity: 'Hours',
            unit_price: 'Rate',
            subtotal: 'Subtotal',
            discount: 'Discount',
            tax: 'VAT',
            total: 'Amount due',
            // The ten document labels (#354) travel in the same object.
            from: 'Issued by',
            invoice_for: 'Billed to',
            invoice_id: 'Invoice no.',
            issue_date: 'Issued',
            due_date: 'Pay by',
            subject: 'Re',
            payment_heading: 'How to pay',
            item_type: 'Kind',
            payments: 'Settlements',
            amount_due: 'Balance due',
        },
    }, 'the preview sends exactly what Save would');
    assert.equal(call.ifNoneMatch, '"preview-etag-1"', 'the conditional request carries the last ETag');
    dom.window.close();
});

test('each render mints one object URL, revokes the one it replaced and releases on teardown', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    const frame = document.getElementById('template-preview-frame');
    const first = frame.getAttribute('src');
    document.getElementById('template-body').value = 'Second render';
    document.getElementById('template-body').dispatchEvent(new window.Event('input', { bubbles: true }));
    await waitFor(() => window.createdUrls.length === 1, { message: 'the debounced render to mint a URL' });
    assert.deepEqual([...window.createdUrls], ['blob:preview-2'], 'one URL per render');
    assert.deepEqual([...window.revokedUrls], ['blob:preview-1'], 'the replaced URL is revoked, not pinned');
    assert.equal(frame.getAttribute('src'), 'blob:preview-2');

    // A second render revokes exactly the URL it replaces.
    document.getElementById('template-body').value = 'Third render';
    await t.renderTemplatePreview();
    assert.deepEqual([...window.createdUrls], ['blob:preview-2', 'blob:preview-3']);
    assert.deepEqual([...window.revokedUrls], ['blob:preview-1', 'blob:preview-2']);
    assert.notEqual(frame.getAttribute('src'), first);

    // Hiding the section is a teardown: the frame and its URL are released.
    document.getElementById('settings-panel-invoice-template').hidden = true;
    await waitFor(() => window.revokedUrls.includes('blob:preview-3'), {
        message: 'hiding the section to revoke the framed URL',
    });
    assert.equal(frame.hasAttribute('src'), false, 'no URL survives the hidden section');
    dom.window.close();
});

test('Open in new tab opens the blob URL currently framed', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    const frame = document.getElementById('template-preview-frame');
    const current = frame.getAttribute('src');
    const opened = [];
    window.open = (url, target) => {
        opened.push({ url, target });
        return { opener: null };
    };
    document.getElementById('template-preview-open').click();
    assert.deepEqual(opened, [{ url: current, target: '_blank' }], 'the tab shows what the preview shows');

    t.releaseTemplatePreview();
    document.getElementById('template-preview-open').click();
    assert.match(document.getElementById('template-preview-status').textContent, /not rendered yet/i,
        'with nothing framed, the control explains instead of opening a dead tab');
    dom.window.close();
});

test('a 422 keeps the last good preview, shows the field errors and retries on the next edit', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    const frame = document.getElementById('template-preview-frame');
    const good = frame.getAttribute('src');
    window.__previewHandler = async () => {
        const err = new Error('payload failed validation');
        err.status = 422;
        err.payload = {
            error: {
                code: 'validation_failed',
                fields: [{ field: 'body', message: 'unknown variable %bogus%' }],
            },
        };
        throw err;
    };
    document.getElementById('template-body').value = 'pay %bogus%';
    document.getElementById('template-body').dispatchEvent(new window.Event('input', { bubbles: true }));
    await waitFor(() => t.calls().preview.length === 1, { message: 'the invalid render' });
    await waitFor(() => document.getElementById('template-error').hidden === false, {
        message: 'the field error to render inline',
    });
    assert.equal(frame.getAttribute('src'), good, 'the last good document stays framed');
    assert.match(document.getElementById('template-error').textContent, /body: unknown variable %bogus%/);
    assert.match(document.getElementById('template-preview-status').textContent, /fix the highlighted fields/i);
    assert.equal(document.getElementById('template-body').value, 'pay %bogus%', 'the edit is never lost');

    window.__previewHandler = null;
    document.getElementById('template-body').value = 'pay %customer_name%';
    document.getElementById('template-body').dispatchEvent(new window.Event('input', { bubbles: true }));
    await waitFor(() => t.calls().preview.length === 2, { message: 'the retry render' });
    await waitFor(() => document.getElementById('template-preview-status').textContent === 'Preview updated', {
        message: 'the retry to succeed',
    });
    assert.equal(t.calls().preview[1].body.body, 'pay %customer_name%', 'the retry carries the newest edit');
    assert.equal(document.getElementById('template-error').hidden, true, 'success clears the field error');
    dom.window.close();
});

test('document-label edits re-render with the new labels, and an org reload drops the ETag (#354)', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    document.getElementById('lb-amount-due').value = 'Total payable';
    document.getElementById('lb-amount-due').dispatchEvent(new window.Event('change', { bubbles: true }));
    await waitFor(() => t.calls().preview.length === 1, {
        message: 'a Document labels edit to request a render',
    });
    const first = t.calls().preview[0];
    assert.equal(first.body.labels.amount_due, 'Total payable', 'the new label rides the preview payload');
    assert.equal(first.body.labels.from, 'Issued by', 'the other document labels travel unchanged');
    await waitFor(() => document.getElementById('template-preview-status').textContent === 'Preview updated', {
        message: 'the label render to finish',
    });

    // Saving Company reloads the org profile; the sample document embeds that
    // identity, so the framed bytes' tag must be dropped and the next render
    // must be unconditional rather than a 304 for the old company block.
    await t.refreshOrgProfile();
    await t.renderTemplatePreview();
    assert.equal(t.calls().preview.length, 2);
    assert.equal(t.calls().preview[1].ifNoneMatch, null, 'a fresh org profile invalidates the preview tag');
    dom.window.close();
});

test('a 304 keeps the framed document and only updates the status', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    const frame = document.getElementById('template-preview-frame');
    const good = frame.getAttribute('src');
    window.__previewHandler = async (method, path, body, opts) => ({
        notModified: true,
        etag: (opts || {}).ifNoneMatch,
        blob: null,
        status: 304,
    });
    document.getElementById('lb-total').value = 'Due now';
    await t.renderTemplatePreview();
    assert.equal(t.calls().preview.length, 1);
    assert.equal(t.calls().preview[0].ifNoneMatch, '"preview-etag-1"');
    assert.equal(frame.getAttribute('src'), good, 'a 304 never blanks the frame');
    assert.deepEqual([...window.createdUrls], [], 'a 304 mints no URL');
    assert.deepEqual([...window.revokedUrls], [], 'a 304 revokes nothing');
    assert.equal(document.getElementById('template-preview-status').textContent, 'Preview updated');
    dom.window.close();
});

test('edits during an in-flight render coalesce into one follow-up with the newest state', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    let release;
    let renders = 0;
    window.__previewHandler = async () => {
        renders += 1;
        if (renders === 1) await new Promise((resolve) => { release = resolve; });
        return { notModified: false, etag: `"etag-${renders}"`, blob: { type: 'application/pdf' }, status: 200 };
    };
    document.getElementById('template-body').value = 'first edit';
    const started = t.renderTemplatePreview();
    // Two more edits while the first response is held open.
    document.getElementById('template-body').value = 'second edit';
    t.renderTemplatePreview();
    document.getElementById('template-body').value = 'newest edit';
    const joined = t.renderTemplatePreview();
    assert.equal(renders, 1, 'only one request is in flight');
    release();
    await started;
    await joined;
    assert.equal(renders, 2, 'exactly one follow-up render, never a storm');
    assert.equal(t.calls().preview.length, 2);
    assert.equal(t.calls().preview[1].body.body, 'newest edit', 'the follow-up carries the newest state');
    assert.deepEqual([...window.createdUrls], ['blob:preview-2', 'blob:preview-3'],
        'the two applied renders each minted one URL');
    assert.deepEqual([...window.revokedUrls], ['blob:preview-1', 'blob:preview-2'],
        'each applied render revoked exactly the URL it replaced');
    dom.window.close();
});

test('the preview card is a titled, labelled, politely announced region', () => {
    const { dom, document } = setup();
    const card = document.getElementById('template-preview');
    assert.ok(card, 'the card always renders: it no longer depends on an invoice existing');
    assert.equal(card.hidden, false);
    assert.equal(document.getElementById('template-preview-subject'), null, 'the retired text preview is gone');
    const frame = document.getElementById('template-preview-frame');
    assert.ok(frame.getAttribute('title').length > 20, 'the frame has a descriptive title');
    const status = document.getElementById('template-preview-status');
    assert.equal(status.getAttribute('role'), 'status', 'the status line is a status region');
    assert.equal(status.getAttribute('aria-live'), 'polite');
    for (const id of ['template-preview-refresh', 'template-preview-open']) {
        const button = document.getElementById(id);
        assert.equal(button.tagName, 'BUTTON', `${id} is a real button`);
        assert.equal(button.getAttribute('type'), 'button');
        assert.ok(button.textContent.trim(), `${id} has a visible label`);
    }
    assert.equal(document.querySelectorAll('#template-preview input, #template-preview select').length, 0,
        'no click-only controls live in the preview card');
    dom.window.close();
});

// ----------------------------------------------------------------- email ----

test('Email prefill never puts the password in the DOM and shows the delivery status', async () => {
    const { dom, document, t } = setup();
    await t.refreshOrgProfile();
    await t.refreshEmailSettings();
    assert.equal(document.getElementById('email-host').value, 'smtp.tucanotime.test');
    assert.equal(document.getElementById('email-port').value, '587');
    assert.equal(document.getElementById('email-username').value, 'mailer');
    assert.equal(document.getElementById('email-from').value, 'billing@tucanotime.test');
    assert.equal(document.getElementById('email-password').value, '', 'the password is never rendered');
    assert.match(document.getElementById('email-password').placeholder, /stored — leave blank to keep/);
    assert.equal(document.getElementById('email-status').textContent, 'Email delivery: SMTP smtp.tucanotime.test:587');
    assert.equal(document.getElementById('email-test-to').value, 'billing@tucanotime.test',
        'the test recipient defaults to the org reply-to');
    assert.equal(document.getElementById('email-form').querySelectorAll('input[disabled]').length, 0,
        'the form is editable when the vault is enabled');
    dom.window.close();
});

test('saving Email sends only changed values and omits a blank password', async () => {
    const { dom, document, t } = setup();
    await t.refreshEmailSettings();
    setValue(document, 'email-host', 'smtp.new.test');
    await t.saveEmailSettings({ preventDefault() { } });
    const call = t.calls().put.find((entry) => entry.path === '/admin/email');
    assert.deepEqual(JSON.parse(JSON.stringify(call.body)), { host: 'smtp.new.test', port: 587 },
        'username/from were unchanged and the blank password is omitted');
    dom.window.close();
});

test('saving Email carries changed credentials and a typed password', async () => {
    const { dom, document, t } = setup();
    await t.refreshEmailSettings();
    setValue(document, 'email-username', 'other');
    setValue(document, 'email-from', 'other@tucanotime.test');
    setValue(document, 'email-password', 's3cret');
    await t.saveEmailSettings({ preventDefault() { } });
    const call = t.calls().put.find((entry) => entry.path === '/admin/email');
    assert.deepEqual(JSON.parse(JSON.stringify(call.body)), {
        host: 'smtp.tucanotime.test',
        port: 587,
        username: 'other',
        from: 'other@tucanotime.test',
        password: 's3cret',
    });
    dom.window.close();
});

test('a disabled vault disables the Email form with an actionable notice', async () => {
    const { dom, document, t } = setup({
        email: { ...EMAIL, vault_enabled: false, configured: false, host: '', username: '', from: '', has_password: false },
    });
    await t.refreshEmailSettings();
    const notice = document.getElementById('email-vault-notice');
    assert.equal(notice.hidden, false, 'the notice is visible');
    assert.match(notice.textContent, /enable it in Security & runtime/i);
    assert.equal(document.getElementById('email-vault-settings').hidden, false,
        'the notice carries the button that routes to the vault card');
    assert.equal(document.getElementById('email-status').textContent,
        'Email delivery: NOT configured — messages are only logged.');
    const controls = [...document.querySelectorAll('#email-form input, #email-form button, #email-test-form input, #email-test-form button')];
    assert.ok(controls.length > 0 && controls.every((control) => control.disabled), 'every control is disabled');
    dom.window.close();
});

test('a 503 vault_disabled on save disables the form and explains it', async () => {
    const { dom, document, t } = setup({
        putError: { status: 503, code: 'vault_disabled', message: 'credential vault is not enabled' },
    });
    await t.refreshEmailSettings();
    setValue(document, 'email-host', 'smtp.other.test');
    await t.saveEmailSettings({ preventDefault() { } });
    const notice = document.getElementById('email-vault-notice');
    assert.equal(notice.hidden, false);
    assert.match(notice.textContent, /enable it in Security & runtime/i);
    assert.equal(document.getElementById('email-error').hidden, false, 'the server error is shown inline');
    assert.equal(document.getElementById('email-error').dataset.errorCode, 'vault_disabled');
    assert.equal(document.getElementById('email-host').disabled, true, 'the form is disabled, not left failing');
    dom.window.close();
});

test('a test send posts once and announces the recipient', async () => {
    const { dom, document, window, t } = setup();
    await t.refreshEmailSettings();
    setValue(document, 'email-test-to', 'qa@tucanotime.test');
    const first = t.sendTestEmail({ preventDefault() { } });
    const second = t.sendTestEmail({ preventDefault() { } });
    await Promise.all([first, second]);
    assert.equal(t.calls().post.length, 1, 'single-flight: a double click sends once');
    assert.deepEqual(JSON.parse(JSON.stringify(t.calls().post[0])), { path: '/admin/email/test', body: { to: 'qa@tucanotime.test' } });
    assert.match(document.getElementById('email-test-result').textContent, /qa@tucano\.test/);
    assert.match(window.document.getElementById('live-region').textContent, /qa@tucano\.test/);
    dom.window.close();
});

test('422 email_not_configured explains the missing transport in the notice', async () => {
    const { dom, document, t } = setup({
        postError: { status: 422, code: 'email_not_configured', message: 'email delivery is not configured' },
    });
    await t.refreshEmailSettings();
    setValue(document, 'email-test-to', 'qa@tucanotime.test');
    await t.sendTestEmail({ preventDefault() { } });
    const error = document.getElementById('email-test-error');
    assert.equal(error.hidden, false);
    assert.match(error.textContent, /not configured/i);
    assert.match(error.textContent, /SMTP host/i);
    assert.equal(error.dataset.errorCode, 'email_not_configured');
    dom.window.close();
});

test('502 email_failed shows the relay failure', async () => {
    const { dom, document, window, t } = setup({
        postError: { status: 502, code: 'email_failed', message: 'relay refused the message' },
    });
    await t.refreshEmailSettings();
    setValue(document, 'email-test-to', 'qa@tucanotime.test');
    await t.sendTestEmail({ preventDefault() { } });
    const error = document.getElementById('email-test-error');
    assert.equal(error.hidden, false);
    assert.match(error.textContent, /relay refused the message/);
    assert.equal(error.dataset.errorCode, 'email_failed');
    assert.match(window.document.getElementById('live-region').textContent, /relay refused/);
    dom.window.close();
});

// ------------------------------------------------- SET-01: the click path --
//
// The old suite called sendTestEmail() directly, so the dead button (a
// `type="button"` with no click listener) went unnoticed. These drive the
// control the user actually presses.

test('SET-01: clicking Send test email issues exactly one POST', async () => {
    const { dom, document, t } = setup();
    await t.refreshEmailSettings();
    setValue(document, 'email-test-to', 'qa@tucanotime.test');
    document.getElementById('email-test-send').click();
    await waitFor(() => t.calls().post.length === 1, { message: 'the clicked button to send' });
    // One event-loop turn: a stray submit listener would have fired by now.
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(t.calls().post.length, 1, 'the click sends once, never twice');
    assert.deepEqual(JSON.parse(JSON.stringify(t.calls().post[0])),
        { path: '/admin/email/test', body: { to: 'qa@tucanotime.test' } });
    dom.window.close();
});

// ------------------------------------------------- SET-02: member gating ---

test('SET-02: a member boot loads no admin surface and announces no role error', async () => {
    const { dom, document, window, t } = setup();
    t.showAccount({ role: 'member', name: 'Mem' });
    assert.equal(t.state.isAdmin, false);
    await t.refreshSettings();
    const paths = t.calls().get.map((path) => String(path));
    assert.equal(paths.filter((path) => path.startsWith('/admin/')).length, 0,
        'no /api/admin/* GET is issued for a member');
    assert.equal(paths.filter((path) => path === '/users').length, 0, 'no /api/users GET is issued for a member');
    assert.equal(document.getElementById('user-list-status').textContent, '',
        'the hidden Users status carries no error text');
    assert.doesNotMatch(window.document.getElementById('live-region').textContent, /administrator role required/i);
    dom.window.close();
});

// ------------------------------------------------- SET-05: double submits --

test('SET-05: the four Settings saves ignore a double submit', async () => {
    const evt = () => ({ preventDefault() { } });

    // Save email settings
    {
        const { dom, document, t } = setup();
        await t.refreshEmailSettings();
        setValue(document, 'email-host', 'smtp.new.test');
        await Promise.all([t.saveEmailSettings(evt()), t.saveEmailSettings(evt())]);
        assert.equal(t.calls().put.filter((call) => call.path === '/admin/email').length, 1,
            'a double click writes the email settings once');
        dom.window.close();
    }

    // Save company info
    {
        const { dom, document, t } = setup();
        await t.refreshOrgProfile();
        setValue(document, 'org-name', 'Tucano Invoice Co');
        await Promise.all([t.saveOrgProfile(evt()), t.saveOrgProfile(evt())]);
        assert.equal(t.calls().put.filter((call) => call.path === '/admin/org').length, 1,
            'a double click writes the org profile once');
        dom.window.close();
    }

    // Save template
    {
        const { dom, document, t } = setup();
        await t.refreshInvoiceTemplate();
        setValue(document, 'lb-total', 'Amount due');
        await Promise.all([t.saveInvoiceTemplate(evt()), t.saveInvoiceTemplate(evt())]);
        assert.equal(t.calls().put.filter((call) => call.path === '/admin/invoice-template').length, 1,
            'a double click writes the template once');
        dom.window.close();
    }

    // Apply to config.json
    {
        const { dom, document, t } = setup();
        await t.refreshConfig();
        setValue(document, 'config-value', '12');
        await Promise.all([t.saveConfig(evt()), t.saveConfig(evt())]);
        assert.equal(t.calls().request.filter((call) => call.method === 'PUT' && call.path === '/admin/config').length, 1,
            'a double click writes the config once');
        dom.window.close();
    }
});

// ------------------------------------------ SET-07: reset re-renders ------

test('SET-07: Use built-in labels re-renders the preview with empty labels', async () => {
    const { dom, document, window, t } = setup();
    await openPreview(t, window, document);
    document.getElementById('lb-reset').click();
    await waitFor(() => t.calls().preview.length === 1, {
        message: 'the label reset to request a fresh preview',
    });
    const labels = t.calls().preview[0].body.labels;
    assert.deepEqual(Object.keys(labels), [
        'description', 'quantity', 'unit_price', 'subtotal', 'discount', 'tax', 'total',
        'from', 'invoice_for', 'invoice_id', 'issue_date', 'due_date',
        'subject', 'payment_heading', 'item_type', 'payments', 'amount_due',
    ], 'every label key travels, including the ten document labels (#354)');
    assert.ok(Object.values(labels).every((value) => value === ''),
        'the preview is rendered from the cleared labels, not the stale frame');
    assert.match(window.document.getElementById('live-region').textContent, /Built-in labels restored/);
    dom.window.close();
});

// --------------------------------- SET-04/SET-08: currency + restart badge --

test('SET-08: clearing the Currency value sends null and the row clears', async () => {
    const { dom, document, t } = setup();
    await t.refreshConfig();
    const select = document.getElementById('config-key');
    select.value = 'base_currency';
    select.dispatchEvent(new dom.window.Event('change'));
    assert.equal(document.getElementById('config-value').value, 'EUR');
    setValue(document, 'config-value', '');
    await t.saveConfig({ preventDefault() { } });
    const call = t.calls().request.find((entry) => entry.path === '/admin/config');
    assert.deepEqual(JSON.parse(JSON.stringify(call.body)), { base_currency: null },
        'an emptied Currency is cleared with null, never an empty string');
    const row = [...document.querySelectorAll('#config-table tbody tr')]
        .find((r) => r.cells[0].textContent === 'base_currency');
    assert.equal(row.cells[1].textContent, '', 'the cleared row renders empty');
    dom.window.close();
});

test('SET-04: boot-time knobs carry the after-restart mark', async () => {
    const { dom, document, t } = setup();
    await t.refreshConfig();
    const rows = [...document.querySelectorAll('#config-table tbody tr')];
    const days = rows.find((r) => r.cells[0].textContent === 'reminder_days');
    const currency = rows.find((r) => r.cells[0].textContent === 'base_currency');
    assert.match(days.cells[3].textContent, /after restart/i, 'reminder_days is read once at boot');
    assert.match(currency.cells[3].textContent, /immediately/i, 'base_currency applies to the next request');
    dom.window.close();
});

// ------------------------------------------------- SET-11: audit card ------

test('SET-11: the audit card lists events, its empty state and an inline error', async () => {
    const { dom, document, window, t } = setup();
    t.showAccount({ role: 'admin', name: 'Admin' });
    await t.refreshAuditLog();
    const rows = [...document.querySelectorAll('#audit-table tbody tr')];
    assert.equal(rows.length, 2, 'both events render, newest first as the API returns them');
    assert.deepEqual([...rows[0].querySelectorAll('td')].map((td) => td.textContent),
        ['2026-10-18 20:00:00 UTC', 'login_ok', 'admin@tucanotime.test']);
    assert.equal(document.getElementById('audit-empty').hidden, true);

    window.__audit = { events: [] };
    await t.refreshAuditLog();
    assert.equal(document.querySelectorAll('#audit-table tbody tr').length, 0);
    assert.equal(document.getElementById('audit-empty').hidden, false, 'the empty state is shown');

    window.__getError = { '/audit': { status: 500, message: 'audit log unreadable' } };
    await t.refreshAuditLog();
    assert.match(document.getElementById('audit-status').textContent, /Could not load the audit log/,
        'a non-403 failure surfaces inline');

    window.__getError = { '/audit': { status: 403, message: 'administrator role required' } };
    await t.refreshAuditLog();
    assert.equal(document.getElementById('audit-status').textContent, '', '403 stays silent');
    assert.equal(document.getElementById('audit-card').hidden, true, '403 hides the admin-only card');
    dom.window.close();
});

// ------------------------------------------------- SET-15: load failures ---

test('SET-15: a 500 on load announces the failure and disables Save', async () => {
    const { dom, document, window, t } = setup({
        getError: { '/admin/org': { status: 500, message: 'corrupt store' } },
    });
    const save = document.getElementById('org-save');
    assert.equal(save.disabled, true, 'Save ships disabled until the stored identity loads');
    await t.refreshOrgProfile();
    assert.equal(save.disabled, true, 'a failed load never enables Save');
    assert.equal(document.getElementById('org-form').dataset.loaded, '0', 'the section is marked not loaded');
    assert.match(window.document.getElementById('live-region').textContent,
        /Company information could not be loaded/, 'the failure is announced, not swallowed');
    dom.window.close();
});

test('SET-15: a 403 load stays silent and keeps Save disabled', async () => {
    const { dom, document, window, t } = setup({
        getError: { '/admin/invoice-template': { status: 403, message: 'administrator role required' } },
    });
    await t.refreshInvoiceTemplate();
    assert.equal(document.getElementById('template-save').disabled, true);
    assert.equal(document.getElementById('template-form').dataset.loaded, '0');
    assert.equal(window.document.getElementById('live-region').textContent, '',
        'a member gets no role error announcement');
    dom.window.close();
});

// ------------------------------------------------------ recipient dialog ----

test('the recipient dialog Settings link names the Email section', () => {
    const { dom, t } = setup();
    assert.equal(t.EMAIL_SETTINGS_ROUTE, '/settings/email');
    dom.window.close();
});

// --------------------------------------------------------------- a11y ------

// The one header shape every Settings view must use: a heading slot, then
// p-cards that each open with a p-card__header (owning exactly one title)
// followed by a p-card__content body. This is the structural half of the
// card-header contract — the CSS half (the shared 1rem lead rhythm that keeps
// the body text below the header rule) is pinned in the next test. Checking
// every card and every panel, rather than the first card only, is what stops a
// future view from reintroducing a per-view header/description variant.
test('every Settings panel uses the heading slot and only headed cards', () => {
    const { dom, document } = setup();
    assert.deepEqual(
        [...document.querySelectorAll('.settings-panel')].map((panel) => panel.id).sort(),
        ROUTES.map(([, , panelId]) => panelId).sort(),
        'every settings panel is listed in ROUTES, so a new panel cannot skip this guard',
    );
    const directTitles = (el) => [...el.children].filter((child) => child.tagName === 'H2');
    for (const [, , panelId] of ROUTES) {
        const panel = document.getElementById(panelId);
        const head = panel.firstElementChild;
        assert.ok(head && head.classList.contains('ts-head'), `${panelId} leads with .ts-head`);
        assert.equal(directTitles(head).length, 1, `${panelId} has exactly one section title in its heading slot`);
        assert.ok(directTitles(head)[0].textContent.trim(), `${panelId}'s section title is named`);
        const cards = [...panel.children].slice(1);
        assert.ok(cards.length > 0, `${panelId} continues with at least one p-card`);
        for (const card of cards) {
            assert.ok(card.classList.contains('p-card'),
                `${panelId} only continues with p-cards, not <${card.tagName.toLowerCase()}>`);
            const header = card.firstElementChild;
            assert.ok(header && header.classList.contains('p-card__header'),
                `${panelId}'s card starts with p-card__header`);
            assert.equal(directTitles(header).length, 1, `${panelId}'s card header owns exactly one h2`);
            const content = header.nextElementSibling;
            assert.ok(content && content.classList.contains('p-card__content'),
                `${panelId}'s card header is followed by p-card__content`);
        }
    }
    dom.window.close();
});

test('the card-body lead rhythm is one shared rule, not a per-view margin', () => {
    const css = readFileSync(new URL('../web/styles.css', import.meta.url), 'utf8')
        .replace(/\/\*[\s\S]*?\*\//g, '');
    // Vanilla ships `[class*=p-card] > p:not([class*=p-heading--]):first-child
    // { margin-top: -.5rem }`. That substring selector also matches
    // `.p-card__content`, and at (0,3,1) it out-specifies the app's
    // `.p-form-help-text { margin: revert }`, so the first paragraph of a card
    // body used to overlap the p-card__header rule (the Company view) while a
    // non-<p> first block (the Users filter bar) did not. The single shared
    // answer must exist, at the same specificity, for every card body.
    assert.match(
        css,
        /\.p-card__content > p:not\(\[class\*="p-heading--"\]\):first-child \{ margin-top: 1rem; \}/,
        'the shared card-body lead rule must keep the first paragraph below the header rule',
    );
    for (const [, , panelId] of ROUTES) {
        assert.equal(
            new RegExp(`#${panelId}[^{]*\\{[^}]*margin-top`).test(css),
            false,
            `${panelId} must not carry a per-view header/lead top-margin patch`,
        );
    }
});

test('every Email control is labelled and the live regions are declared', () => {
    const { dom, document } = setup();
    for (const control of document.querySelectorAll('#email-form input, #email-test-form input')) {
        assert.equal(control.labels.length, 1, `${control.id} has exactly one label`);
        assert.ok(control.labels[0].textContent.trim(), `${control.id} has an accessible name`);
        assert.ok(control.labels[0].classList.contains('p-form__label'), `${control.id}'s label is official`);
        assert.ok(control.classList.contains('p-form__control'), `${control.id} is an official control`);
    }
    for (const id of ['email-status', 'email-test-result']) {
        const region = document.getElementById(id);
        assert.equal(region.getAttribute('role'), 'status', `${id} is a status region`);
        assert.equal(region.getAttribute('aria-live'), 'polite', `${id} announces politely`);
    }
    for (const id of ['email-vault-notice', 'email-error', 'email-test-error']) {
        assert.equal(document.getElementById(id).getAttribute('role'), 'alert', `${id} is an alert`);
    }
    dom.window.close();
});

test('the Vault key card declares its live regions and named actions', async () => {
    const { dom, document, t } = setup();
    const status = document.getElementById('vault-status');
    assert.equal(status.getAttribute('role'), 'status', 'the state line is a status region');
    assert.equal(status.getAttribute('aria-live'), 'polite', 'the state line announces politely');
    for (const id of ['vault-warning', 'vault-error']) {
        assert.equal(document.getElementById(id).getAttribute('role'), 'alert', `${id} is an alert`);
        assert.equal(document.getElementById(id).hidden, true, `${id} starts hidden`);
    }
    for (const id of ['vault-enable', 'vault-regenerate', 'vault-disable']) {
        const button = document.getElementById(id);
        assert.equal(button.tagName, 'BUTTON', `${id} is a real button`);
        assert.equal(button.getAttribute('type'), 'button', `${id} never submits a form`);
        assert.ok(button.textContent.trim(), `${id} has an accessible name`);
        assert.equal(button.hidden, true, `${id} is hidden until the status loads`);
    }
    await t.refreshVault();
    assert.equal(document.getElementById('vault-enable').hidden, true,
        'an enabled key-file vault offers no Enable action');
    assert.equal(document.getElementById('vault-regenerate').hidden, false);
    assert.equal(document.getElementById('vault-regenerate').disabled, false);
    assert.ok(document.getElementById('vault-regenerate').textContent.trim(), 'the visible action is named');
    dom.window.close();
});

test('the Settings tab strip navigates with the horizontal key contract', () => {
    const { dom, document } = setup();
    document.getElementById('panel-settings').hidden = false;
    const press = (key) => document.activeElement.dispatchEvent(
        new dom.window.KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }),
    );
    document.getElementById('settings-tab-users').focus();
    press('ArrowRight');
    assert.equal(document.activeElement.id, 'settings-tab-company', 'ArrowRight → next section');
    press('End');
    assert.equal(document.activeElement.id, 'settings-tab-security', 'End → last section');
    press('Home');
    assert.equal(document.activeElement.id, 'settings-tab-users', 'Home → first section');
    press('ArrowLeft');
    assert.equal(document.activeElement.id, 'settings-tab-security', 'ArrowLeft wraps to the last');
    press('ArrowDown');
    assert.equal(document.activeElement.id, 'settings-tab-security', 'ArrowDown is inert on a horizontal tablist');
    dom.window.close();
});
