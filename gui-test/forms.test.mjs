// #232 phase 2b: the app's forms are the official Vanilla form patterns. The
// bespoke `.field` wrapper, `.hint` and `.form-error` skins are retired for
// `p-form__group` / `p-form__label` / `p-form__control` / `p-form-help-text` /
// `p-form-validation__message` (in a `p-form-validation is-error` wrapper), and
// every button-creation path now states its own `data-icon`/`data-positive`
// instead of relying on `decorateUbuntuButton`'s label/type/id fallbacks, which
// phase 2e then deleted outright. This
// offline jsdom suite (same harness as registers.test.mjs / chips.test.mjs)
// pins the vocabulary, the label<->control pairing, the preserved field
// contract, the CSS that left the app sheet and the button attribute coverage.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { JSDOM } from 'jsdom';
import { loadAppSource } from './load-app.mjs';

const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const app = loadAppSource();
const stylesCss = readFileSync(new URL('../web/styles.css', import.meta.url), 'utf8');
const vendorCss = readFileSync(new URL('../web/vendor/ubuntu/components.css', import.meta.url), 'utf8');
// Selectors only: comments carry the "#232 phase 2b" rationale and do mention
// the retired classes.
const stylesRules = stylesCss.replace(/\/\*[\s\S]*?\*\//g, '');

// The 25 form/panel error messages (`<p id="…-error" role="alert">`), now the
// official validation message inside an `is-error` group. `#invoice-error` left
// with the legacy direct draft generator (#349); `#aw-error` arrived with the
// admin startup wizard; `#send-email-error` (invalid extras) and
// `#send-email-notice` (SMTP not configured) arrived with the invoice
// recipient dialog.
const ERROR_IDS = [
    'auth-error', 'entry-error', 'iw-error', 'ed-error',
    'rec-error', 'ret-error', 'expense-error', 'expense-category-error',
    'submission-error', 'user-error', 'user-password-error', 'secret-error', 'vault-error',
    'config-error', 'category-error', 'org-error', 'template-error',
    'item-error', 'wz-error', 'customer-error', 'project-error', 'task-error',
    'week-row-error', 'aw-error', 'send-email-error', 'send-email-notice',
    'email-error', 'email-test-error',
];

function shell() {
    const dom = new JSDOM(html, { url: 'http://localhost/' });
    return { dom, document: dom.window.document };
}

function setup() {
    const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
    dom.window.HTMLDialogElement.prototype.showModal = function () { this.open = true; };
    dom.window.HTMLDialogElement.prototype.close = function () { this.open = false; };
    const stub = `
window.f = {
  api, state, refreshCatalog, iwLoadProjects, fillSelect, $, decorateUbuntuButton,
  setPaymentRails, readPaymentRails, addPaymentRail, removePaymentRail, paymentRailRows,
    validatePaymentRails, railHasContent, saveCustomer, showFormError, startCustomerEdit, cancelCustomerEdit,
};`;
    dom.window.eval(`${app}\n${stub}`);
    return { dom, document: dom.window.document, f: dom.window.f };
}

// ------------------------------------------------------------- vocabulary --

test('the shell ships the official form vocabulary and no bespoke class (#232 phase 2b)', () => {
    const { dom, document } = shell();
    const groups = document.querySelectorAll('.p-form__group');
    // #354 added the Company legal-details fieldset, the payment-rails
    // fieldset and the ten Document label rows (each in its own grouped
    // fieldset) to the Settings split's 146 groups; the template notes row
    // added one more, and the Expense-categories dialog one more.
    assert.equal(groups.length, 169, 'every field wrapper is a p-form__group');
    assert.equal(document.querySelectorAll('.p-form__label').length, 161, 'one official label per field row');
    assert.equal(document.querySelectorAll('.p-form__control').length, 161, 'one official control per field row');
    // The previous 55 plus the template footer and notes placement hints.
    assert.equal(document.querySelectorAll('.p-form-help-text').length, 57, 'the hints became p-form-help-text');
    assert.equal(document.querySelectorAll('.p-form-validation__message').length, ERROR_IDS.length,
        'every error message is the official validation message');
    assert.equal(document.querySelectorAll('.p-form-validation.is-error').length, ERROR_IDS.length,
        'each validation message sits in an is-error group');
    assert.equal(document.querySelectorAll('.field').length, 0, 'no .field wrapper survives');
    assert.equal(document.querySelectorAll('.hint').length, 0, 'no .hint survives');
    assert.equal(document.querySelectorAll('.form-error').length, 0, 'no .form-error survives');
    // The filter bars and the dialog prompt stay app-owned on purpose: they are
    // horizontal/compact controls, not stacked form rows. 11 = the hierarchy
    // registers (1 projects + 2 tasks + 2 expenses) plus the six invoice filter
    // fields (status, search, customer, project, from, to).
    assert.equal(document.querySelectorAll('.filter-field').length, 11, 'the compact filter controls stay app-owned');
    dom.window.close();
});

test('every p-form__group pairs its label with its own control (#232 phase 2b)', () => {
    const { dom, document } = shell();
    const groups = [...document.querySelectorAll('.p-form__group')];
    let paired = 0;
    const composed = [];
    for (const group of groups) {
        const label = [...group.children].find((child) => child.tagName === 'LABEL');
        if (!label) {
            // The composed rows carry no label of their own: the seven grouped
            // fieldsets (their rows are nested groups) and the "Contacts" row,
            // whose caption is a span referenced by the fieldset heading.
            // Keep the tag and the id apart: #354 gave three of the fieldsets
            // ids, and the census counts elements, not their rendered selector.
            composed.push({ tag: group.tagName, id: group.id });
            continue;
        }
        assert.ok(label.classList.contains('p-form__label'), `label in ${group.id || 'a group'} is p-form__label`);
        const control = label.control || document.getElementById(label.getAttribute('for'));
        assert.ok(control, `label ${label.getAttribute('for')} resolves to a control`);
        assert.ok(group.contains(control), `label ${label.getAttribute('for')} targets a control in its own group`);
        assert.ok(control.classList.contains('p-form__control'), `${control.id} is p-form__control`);
        assert.equal(control.labels.length, 1, `${control.id} keeps exactly one label association`);
        assert.equal(control.labels[0], label, `${control.id} keeps its original label`);
        assert.ok(label.textContent.trim().length > 0, `${control.id} keeps an accessible name`);
        paired += 1;
    }
    // 152: the Settings split's 141 plus the ten Document label rows (#354)
    // and the template notes row; the Expense-categories dialog adds one.
    assert.equal(paired, 161, 'all 161 label/control pairs were verified');
    assert.equal(composed.length, 8, 'eight composed rows have no label of their own');
    assert.equal(composed.filter((row) => row.tag === 'FIELDSET').length, 7, 'seven grouped fieldsets');
    assert.equal(composed.filter((row) => row.tag === 'DIV').length, 1, 'the Contacts row');
    dom.window.close();
});

// ------------------------------------------------- preserved field contract --

test('the migrated fields keep their ids, order and validation attributes (#232 phase 2b)', () => {
    const { dom, document } = shell();
    const row = (group) => [
        group.querySelector('label').getAttribute('for'),
        group.querySelector('.p-form__control').id,
        group.querySelector('.p-form__control').tagName,
    ];
    const entry = [...document.querySelectorAll('#entry-form .p-form__group')].map(row);
    assert.deepEqual(entry, [
        ['entry-customer', 'entry-customer', 'SELECT'],
        ['entry-project', 'entry-project', 'SELECT'],
        ['entry-task', 'entry-task', 'SELECT'],
        ['entry-hours', 'entry-hours', 'INPUT'],
        ['entry-note', 'entry-note', 'TEXTAREA'],
        ['entry-billable', 'entry-billable', 'INPUT'],
    ], 'the Day-entry dialog keeps its field order and labels');
    const weekRow = [...document.querySelectorAll('#week-row-form .p-form__group')].map(row);
    assert.deepEqual(weekRow, [
        ['week-row-customer', 'week-row-customer', 'SELECT'],
        ['week-row-project', 'week-row-project', 'SELECT'],
        ['week-row-task', 'week-row-task', 'SELECT'],
    ], 'the Week add-row dialog asks only customer/project/task');
    const auth = [...document.querySelectorAll('#auth-form .p-form__group')].map(row);
    assert.deepEqual(auth, [
        ['auth-name', 'auth-name', 'INPUT'],
        ['auth-email', 'auth-email', 'INPUT'],
        ['auth-password', 'auth-password', 'INPUT'],
    ]);
    // The attributes the migration must not have touched.
    const hours = document.getElementById('entry-hours');
    assert.deepEqual([hours.type, hours.min, hours.max, hours.step, hours.required, hours.getAttribute('inputmode')],
        ['number', '0.01', '24', '0.01', true, 'decimal']);
    assert.deepEqual([hours.name, hours.getAttribute('aria-invalid')], ['hours', null]);
    const note = document.getElementById('entry-note');
    assert.deepEqual([note.maxLength, note.rows, note.name], [500, 2, 'note']);
    const email = document.getElementById('auth-email');
    assert.deepEqual([email.type, email.autocomplete, email.required], ['email', 'username', true]);
    const accent = document.getElementById('org-accent');
    assert.deepEqual([accent.getAttribute('pattern'), accent.maxLength, accent.placeholder],
        ['^#[0-9a-fA-F]{6}$', 7, '#E95420'], 'the accent pattern/maxlength survive');
    const termsDays = document.getElementById('template-terms-days');
    assert.deepEqual([termsDays.type, termsDays.min, termsDays.max, termsDays.step], ['number', '1', '365', '1']);
    const notes = document.getElementById('template-notes');
    assert.deepEqual([notes.tagName, notes.maxLength, notes.rows], ['TEXTAREA', 4000, 3],
        'the template notes field is a bounded textarea');
    assert.match(document.getElementById('template-notes-help').textContent, /%variables%/);
    assert.match(document.getElementById('template-footer-help').textContent, /before the Notes/);
    const currency = document.getElementById('expense-currency');
    assert.deepEqual([currency.maxLength, currency.placeholder, currency.required], [3, 'EUR', true]);
    dom.window.close();
});

// ---------------------------------------------------------------- the CSS --

test('web/styles.css no longer owns the form skins, and keeps the app-owned rhythm (#232 phase 2b)', () => {
    for (const selector of ['\\.field\\b', '\\.hint\\b', '\\.form-error\\b']) {
        assert.equal(new RegExp(selector).test(stylesRules), false,
            `${selector.replace(/\\\\/g, '')} must not survive as a selector`);
    }
    // The classes the app switched to really are in the vendored bundle.
    for (const cls of ['p-form__group', 'p-form__label', 'p-form__control', 'p-form-help-text', 'p-form-validation__message']) {
        assert.ok(vendorCss.includes(cls), `${cls} comes from the vendored Vanilla build`);
    }
    // App-owned rhythm Vanilla does not express: a constant .65rem row rhythm
    // (Vanilla spaces groups only inside .p-form--stacked, at >= 1036px), the
    // tick-box size the base width:100% rule does not cover, the fieldset frame
    // that overrides Vanilla's tinted/clipping base fieldset, and the help-text
    // margins that the bundle would otherwise pull up (.p-form-help-text sits
    // after headings in this app, not after an input).
    assert.ok(/\.p-form__group \{ margin-bottom: \.65rem; \}/.test(stylesRules), 'the group row rhythm');
    assert.ok(/\.p-form__group input\[type="checkbox"\], \.theme-switch input \{ min-height: 0; width: 1rem; height: 1rem; \}/.test(stylesRules),
        'the tick-box size');
    assert.ok(/fieldset\.p-form__group \{/.test(stylesRules), 'the fieldset frame');
    assert.ok(/fieldset\.p-form__group > legend \{/.test(stylesRules), 'the fieldset legend');
    assert.ok(/\.customer-extra-details fieldset\.p-form__group \{ margin-top: \.5rem; \}/.test(stylesRules),
        'the nested fieldset rhythm');
    assert.ok(/\.p-form-help-text \{ margin: revert; \}/.test(stylesRules), 'the help-text margin override');
    assert.ok(/#wizard-dialog \.p-form__group:last-child \{ margin-bottom: 0; \}/.test(stylesRules),
        'the wizard keeps no trailing gap before its action row');
});

// --------------------------------------------------------------- errors ----

test('every error stays a role=alert validation message while hidden (#232 phase 2b)', () => {
    const { dom, document } = shell();
    const seen = [];
    for (const id of ERROR_IDS) {
        const el = document.getElementById(id);
        assert.ok(el, `#${id} exists`);
        assert.equal(el.getAttribute('role'), 'alert', `#${id} stays role=alert`);
        assert.equal(el.className, 'p-form-validation__message', `#${id} is the official message`);
        assert.equal(el.hidden, true, `#${id} ships hidden (the JS reveals it)`);
        const wrapper = el.parentElement;
        assert.equal(wrapper.className, 'p-form-validation is-error', `#${id} sits in an is-error group`);
        assert.equal(el.getAttribute('aria-describedby'), null);
        seen.push(id);
    }
    assert.deepEqual(seen, ERROR_IDS, 'the error set is unchanged (ids are the JS contract)');
    // #email-vault-notice and the vault card's #vault-warning are plain
    // role="alert" explanations (not form validation messages), so they are the
    // two extra alert nodes beyond ERROR_IDS.
    assert.equal(document.querySelectorAll('[role="alert"]').length, ERROR_IDS.length + 2,
        'the email vault notice and the vault warning are the only extra role=alert nodes');
    dom.window.close();
});

// ------------------------------------------------- button attribute contract --
//
// #232 phase 2e: `decorateUbuntuButton` no longer infers anything — its old
// submit-type / class / id-allowlist / label-matching fallbacks (and the
// label-mutating "+"-strip they brought) are deleted, so decoration is a pure
// function of `data-icon` and `data-positive`. This guard asserts both halves of
// that contract: (1) the decorator adds nothing when the two attributes are
// absent, so reintroducing a fallback fails here; and (2) no rendered button
// relies on the deleted fallback — every button that needs a colour or a glyph
// states it, with the coverage pinned so dropping an attribute also fails.

test('decoration is attribute-only and every rendered button states what it needs (#232 phase 2e)', () => {
    const { dom, document, f } = setup();
    const label = (b) => b.id || b.textContent.trim() || b.outerHTML.slice(0, 40);

    // (1) No inference. Decorate a clone of every shell button with its
    // decoration attributes stripped: nothing may be re-derived from the label,
    // the type or the id.
    for (const button of document.querySelectorAll('button')) {
        if (button.getAttribute('role') === 'tab' || button.classList.contains('p-chip__dismiss')) continue;
        const clone = button.cloneNode(true);
        clone.removeAttribute('data-icon');
        clone.removeAttribute('data-positive');
        clone.classList.remove('p-button', 'p-button--positive');
        clone.querySelectorAll('i[class*="p-icon--"]').forEach((icon) => icon.remove());
        assert.equal(clone.querySelector('[class*="p-icon--"]'), null, 'the clone starts with no icon');
        f.decorateUbuntuButton(clone);
        assert.equal(clone.querySelector('[class*="p-icon--"]'), null,
            `${label(button)} must not gain an icon without data-icon`);
        assert.equal(clone.classList.contains('p-button--positive'), false,
            `${label(button)} must not become positive without data-positive`);
        if (!clone.classList.contains('link') && !clone.classList.contains('danger')) {
            assert.ok(clone.classList.contains('p-button'), `${label(button)} still gets the neutral skin`);
        }
    }

    // (2) Coverage. A form submit control is always the primary action here, so
    // it must say so; the legacy `.primary` class may decorate a button but must
    // never be the only positive signal (that fallback is deleted).
    const submits = [...document.querySelectorAll('button[type="submit"]')];
    assert.ok(submits.length > 0, 'the shell has form submit controls');
    for (const b of submits) assert.equal(b.dataset.positive, '1', `submit ${label(b)} states data-positive`);
    for (const b of document.querySelectorAll('button.primary')) {
        assert.equal(b.dataset.positive, '1', `.primary button ${label(b)} also states data-positive`);
    }

    // Pinned coverage: dropping a data-* attribute from any shell button makes
    // one of these counts drop and fails the guard. (The HTML comment that
    // mentions `data-icon="plus"` is a comment node, not a button.)
    const icons = [...document.querySelectorAll('button[data-icon]')];
    assert.equal(icons.length, 21, 'every icon-bearing shell button states data-icon');
    const vocab = {};
    for (const b of icons) vocab[b.dataset.icon] = (vocab[b.dataset.icon] || 0) + 1;
    // The newest plus is Settings → Expense categories' heading action.
    assert.deepEqual(vocab, { plus: 14, copy: 2, play: 2, stop: 1, history: 1, file: 1 }, 'the shell icon vocabulary is pinned');
    // 37: the previous 36 plus that same + Category heading action.
    // The count is pinned so a new positive action updates this guard on
    // purpose: `+ Manual Invoice` is the secondary action and deliberately
    // carries no data-positive, and each dialog's `Cancel`/`Close` is a plain
    // secondary button, so neither adds to it.
    assert.equal(document.querySelectorAll('button[data-positive]').length, 37,
        'every positive shell button states data-positive');

    // The deleted label-mutation leaves no trace: no rendered label keeps a "+".
    assert.equal(/<button[^>]*>\s*\+/.test(html), false, 'no button label carries a leading "+"');
    dom.window.close();
});

test('the JS builders emit the official fields and explicit button icons (#232 phase 2b)', async () => {
    const { dom, document, f } = setup();
    f.api.get = async (path) => {
        if (path === '/customers/c1') return { customer: { id: 'c1', name: 'Tucano Time', currency: 'EUR' } };
        if (path.startsWith('/entries?')) return { entries: [{ id: 'e1', customer_id: 'c1', billable: true, project_code: 'SM-1', hours: 2 }] };
        if (path === '/invoices') return { invoices: [] };
        if (path === '/customers/c1/projects') return { projects: [{ code: 'SM-1', name: 'Smoke', active: true }] };
        return {};
    };
    // A <select> ignores a .value with no matching option, so fill it first.
    f.fillSelect(f.$('iw-customer'), [{ value: 'c1', text: 'Tucano Time' }]);
    f.$('iw-from').value = '2026-10-01';
    f.$('iw-to').value = '2026-10-31';
    await f.iwLoadProjects();
    const group = document.querySelector('#iw-projects .p-form__group');
    assert.ok(group, 'the generated project tick box is a p-form__group');
    assert.equal(document.querySelector('#iw-projects .field'), null, 'the generated markup is not the bespoke .field');
    const label = group.querySelector('label.p-form__label');
    const box = group.querySelector('input.p-form__control');
    assert.equal(label.getAttribute('for'), 'iw-p-SM-1');
    assert.equal(box.id, 'iw-p-SM-1');
    assert.equal(box.type, 'checkbox');
    assert.equal(label.control, box, 'the generated label still owns its control');

    // The empty state is the official help text, not the bespoke hint.
    f.api.get = async (path) => (path === '/invoices' ? { invoices: [] }
        : path === '/customers/c1/projects' ? { projects: [] }
            : path === '/customers/c1' ? { customer: { id: 'c1' } } : { entries: [] });
    await f.iwLoadProjects();
    assert.equal(document.querySelector('#iw-projects .p-form-help-text').textContent,
        'No uninvoiced billable work in this period.');

    // Row actions state their own icon (they used to be derived from the label).
    f.api.get = async () => ({ item_types: [{ id: 'i1', name: 'Consulting', kind: 'service', description: '', default_price_minor: 12500, currency: 'EUR', active: true }] });
    await f.refreshCatalog();
    const actions = [...document.querySelectorAll('#item-table tbody button')];
    assert.deepEqual(actions.map((b) => b.dataset.icon), ['edit', undefined, 'delete'],
        'the Edit/Delete row actions state their icon; Archive/Restore has none');
    dom.window.close();
});

// -------------------------------------------------- payment rails (#354) ----

test('payment-rail add/remove produces the documented PaymentDetails.rails payload (#354)', async () => {
    const { dom, document, f } = setup();
    const posted = [];
    f.api.get = async (path) => (path === '/customers' ? { customers: [] } : {});
    f.api.post = async (path, body) => { posted.push({ path, body }); return body; };
    f.api.put = async (path, body) => { posted.push({ path, body }); return body; };

    f.setPaymentRails([]);
    const add = document.getElementById('pay-rail-add');
    assert.equal(add.tagName, 'BUTTON', 'the add control is a real button');
    f.addPaymentRail();
    const rows = () => f.paymentRailRows();
    assert.equal(rows().length, 1, 'a real button adds a rail');
    const set = (id, value) => { document.getElementById(id).value = value; };
    set('pay-rail-0-currency', 'eur');
    set('pay-rail-0-account-name', 'Tucano Time EUR');
    set('pay-rail-0-iban', 'EX000000000000000000');
    set('pay-rail-0-bank-name', 'Tucano Bank');
    set('pay-rail-0-instructions', 'Quote the invoice number');

    const read = f.readPaymentRails();
    assert.equal(read[0].currency, 'EUR', 'the currency is upper-cased on read');
    assert.equal(read[0].bank_name, 'Tucano Bank');
    assert.equal(f.validatePaymentRails(read), null, 'a well-formed rail validates');

    // A second rail, then remove it: the route back to zero works.
    f.addPaymentRail();
    set('pay-rail-1-currency', 'USD');
    assert.equal(rows().length, 2);
    f.removePaymentRail(rows()[1]);
    assert.equal(rows().length, 1, 'remove deletes that row');

    // An untouched blank row is neither validated nor persisted.
    f.addPaymentRail();
    assert.equal(rows().length, 2);
    assert.equal(f.railHasContent(f.readPaymentRails()[1]), false);

    // Save the customer and inspect the exact rails payload.
    set('customer-name', 'Rails Co');
    set('customer-currency', 'EUR');
    set('customer-rate', '90');
    await f.saveCustomer({ preventDefault() { } });
    const body = posted[0].body;
    assert.equal(posted[0].path, '/customers');
    assert.deepEqual(JSON.parse(JSON.stringify(body.payment_details.rails)), [{
        currency: 'EUR',
        account_name: 'Tucano Time EUR',
        iban: 'EX000000000000000000',
        bank_name: 'Tucano Bank',
        instructions: 'Quote the invoice number',
    }], 'the payload carries the rail with its optional fields and no blank row');
    dom.window.close();
});

test('payment-rail validation surfaces the API field path against the right input (#354)', () => {
    const { dom, document, f } = setup();
    f.setPaymentRails([
        { currency: 'EUR', account_name: 'Tucano' },
        { currency: '', account_name: 'No currency' },
    ]);
    f.addPaymentRail(); // an untouched third row must not mask the error
    const problem = f.validatePaymentRails(f.readPaymentRails());
    assert.ok(problem, 'a rail without a currency is refused');
    assert.equal(problem.payload.error.fields[0].field, 'payment_details.rails[1].currency');

    // The real shared showFormError maps that field path to the second visible
    // row's control, not to a name no input owns.
    f.showFormError(document.getElementById('customer-error'), problem);
    const control = document.getElementById('pay-rail-1-currency');
    assert.equal(control.getAttribute('aria-invalid'), 'true', 'the second visible row is marked');
    assert.match(control.getAttribute('aria-describedby'), /customer-error/);
    assert.match(document.getElementById('customer-error').textContent,
        /payment_details\.rails\[1\]\.currency: required/);

    // The rail's account name is required, like the API's own validator: an
    // otherwise complete rail with a blank one is refused before any request.
    f.setPaymentRails([{ currency: 'EUR', account_name: '   ', iban: 'EX000000000000000000' }]);
    const blankName = f.validatePaymentRails(f.readPaymentRails());
    assert.equal(blankName.payload.error.fields[0].field, 'payment_details.rails[0].account_name');

    // The flat single-rail block maps too: `payment_details.iban` is #pay-iban,
    // not #customer-payment-details.iban.
    f.showFormError(document.getElementById('customer-error'), {
        message: 'payment_details.iban: must be 5-34 letters/digits',
        payload: { error: { code: 'validation_failed', fields: [{ field: 'payment_details.iban', message: 'must be 5-34 letters/digits' }] } },
    });
    assert.equal(document.getElementById('pay-iban').getAttribute('aria-invalid'), 'true');
    dom.window.close();
});

test('customer invoice prefix is loaded, saved, cleared and reset between customers (#392)', async () => {
    const { dom, document, f } = setup();
    const saved = [];
    f.api.get = async () => ({ customers: [] });
    f.api.put = async (path, body) => { saved.push({ path, body }); return body; };
    f.api.post = async (path, body) => { saved.push({ path, body }); return body; };
    const customer = { id: 'c1', name: 'Prefix customer', currency: 'EUR', default_rate_minor: 100, active: true, invoice_prefix: 'MONKEY' };
    const field = document.getElementById('customer-invoice-prefix');
    f.startCustomerEdit(customer);
    assert.equal(field.value, 'MONKEY');
    field.value = 'CUST1';
    await f.saveCustomer({ preventDefault() { } });
    assert.equal(saved[0].path, '/customers/c1');
    assert.equal(saved[0].body.invoice_prefix, 'CUST1');
    assert.equal(field.value, '', 'closing the saved form resets the prefix');
    f.startCustomerEdit(customer);
    field.value = '';
    await f.saveCustomer({ preventDefault() { } });
    assert.equal(saved[1].body.invoice_prefix, '', 'an empty field explicitly clears the prefix');
    const legacy = { ...customer };
    delete legacy.invoice_prefix;
    f.startCustomerEdit(legacy);
    assert.equal(field.value, '', 'legacy customers have no custom prefix');
    f.cancelCustomerEdit();
    document.getElementById('customer-name').value = 'New customer';
    document.getElementById('customer-currency').value = 'EUR';
    document.getElementById('customer-rate').value = '1';
    field.value = 'CUST2';
    await f.saveCustomer({ preventDefault() { } });
    assert.equal(saved[2].path, '/customers');
    assert.equal(saved[2].body.invoice_prefix, 'CUST2');
    dom.window.close();
});
