import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { JSDOM } from 'jsdom';

const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const app = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
const user = { id: 'test-user', name: 'Test user' };

function setup() {
  const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
  dom.window.HTMLDialogElement.prototype.showModal = function () { this.open = true; };
  dom.window.HTMLDialogElement.prototype.close = function () { this.open = false; };
  dom.window.eval(`${app}\nwindow.userTest = { api, changeUserPassword, closeUserPassword, saveUserPassword, newUserForm, closeUserForm, saveUser };`);
  const context = dom.window.userTest;
  context.api.get = async () => ({ users: [] });
  return { dom, context, document: dom.window.document };
}

test('late password response does not close or erase another draft; duplicate submit ignored', async () => {
  const { dom, context, document } = setup();
  let resolve;
  let writes = 0;
  context.api.put = () => { writes += 1; return new Promise((done) => { resolve = done; }); };
  context.changeUserPassword(user);
  document.getElementById('user-new-password').value = 'first-password';
  const pending = context.saveUserPassword({ preventDefault() {} });
  await context.saveUserPassword({ preventDefault() {} });
  assert.equal(writes, 1);
  context.closeUserPassword();
  context.changeUserPassword({ id: 'other', name: 'Other' });
  document.getElementById('user-new-password').value = 'other-draft';
  resolve();
  await pending;
  assert.equal(document.getElementById('user-password-dialog').open, true);
  assert.equal(document.getElementById('user-new-password').value, 'other-draft');
  dom.window.close();
});

test('authentication failure clears both password inputs and returns to sign in', async () => {
  const { dom, context, document } = setup();
  context.api.put = async () => { const error = new Error('Session expired'); error.status = 401; throw error; };
  document.getElementById('user-password').value = 'initial-secret';
  context.changeUserPassword(user);
  document.getElementById('user-new-password').value = 'changed-secret';
  await context.saveUserPassword({ preventDefault() {} });
  assert.equal(document.getElementById('user-password').value, '');
  assert.equal(document.getElementById('user-new-password').value, '');
  assert.equal(document.getElementById('user-password-dialog').open, false);
  assert.equal(document.getElementById('auth-overlay').hidden, false);
  dom.window.close();
});

test('late create response does not close a reopened user draft', async () => {
  const { dom, context, document } = setup();
  let resolve;
  context.api.post = () => new Promise((done) => { resolve = done; });
  context.newUserForm();
  document.getElementById('user-dialog').showModal();
  document.getElementById('user-password').value = 'initial-password';
  const pending = context.saveUser({ preventDefault() {} });
  context.closeUserForm();
  context.newUserForm();
  document.getElementById('user-dialog').showModal();
  document.getElementById('user-name').value = 'Another draft';
  resolve();
  await pending;
  assert.equal(document.getElementById('user-dialog').open, true);
  assert.equal(document.getElementById('user-name').value, 'Another draft');
  dom.window.close();
});