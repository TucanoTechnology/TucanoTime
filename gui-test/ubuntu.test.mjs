import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { parse, walk, generate } from 'css-tree';
import { JSDOM } from 'jsdom';

const vendor = new URL('../web/vendor/ubuntu/', import.meta.url);
const css = readFileSync(new URL('components.css', vendor), 'utf8');
const manifest = JSON.parse(readFileSync(new URL('manifest.json', vendor), 'utf8'));

test('pinned official components and local Ubuntu fonts match recorded hashes', () => {
  assert.equal(manifest.version, '4.59.0');
  assert.equal(createHash('sha256').update(css).digest('hex'), manifest.cssSha256);
  assert.equal(manifest.assets.length, 3);
  for (const asset of manifest.assets) {
    const bytes = readFileSync(new URL(asset.filename, vendor));
    assert.equal(bytes.subarray(0, 4).toString(), 'wOF2');
    assert.equal(createHash('sha256').update(bytes).digest('hex'), asset.sha256);
  }
  assert.ok(existsSync(new URL('VANILLA-LICENSE', vendor)));
  assert.ok(existsSync(new URL('GPL-3.0.txt', vendor)));
  assert.ok(existsSync(new URL('UBUNTU-FONT-LICENSE.txt', vendor)));
  assert.ok(existsSync(new URL('vanilla-framework-4.59.0.tgz', vendor)));
});

test('component stylesheet has no runtime remote asset dependency', () => {
  walk(parse(css), (node) => {
    if (node.type === 'Url') assert.ok(node.value.startsWith('/vendor/ubuntu/') || node.value.startsWith('data:'));
  });
  assert.match(css, /font-display:swap/);
  assert.match(css, /p-button--positive/);
});

test('static official icons are decorative and icon-only controls have names', () => {
  const document = new JSDOM(readFileSync(new URL('../web/index.html', import.meta.url), 'utf8')).window.document;
  for (const icon of document.querySelectorAll('[class*="p-icon--"]')) {
    assert.equal(icon.getAttribute('aria-hidden'), 'true');
    const button = icon.closest('button');
    if (button && !button.textContent.trim()) assert.ok(button.getAttribute('aria-label') && button.title);
  }
  assert.ok(!document.getElementById('wizard-open').textContent.includes('🚀'));
});

test('official positive buttons meet AA text contrast in both framework themes', () => {
  const themes = [];
  walk(parse(css), { visit: 'Rule', enter(node) {
    const values = {};
    node.block.children.forEach((declaration) => {
      if (declaration.type === 'Declaration') values[declaration.property] = generate(declaration.value);
    });
    if (values['--vf-color-button-positive-default']) themes.push(values);
  } });
  assert.ok(themes.length >= 2);
  const colourDom = new JSDOM('<span></span>');
  const swatch = colourDom.window.document.querySelector('span');
  const luminance = (colour) => {
    swatch.style.color = colour;
    const normalized = colourDom.window.getComputedStyle(swatch).color;
    const channels = normalized.match(/[\d.]+/g).slice(0, 3).map((channel) => Number(channel) / 255)
      .map((value) => value <= .04045 ? value / 12.92 : ((value + .055) / 1.055) ** 2.4);
    return channels[0] * .2126 + channels[1] * .7152 + channels[2] * .0722;
  };
  for (const values of themes) {
    const levels = [luminance(values['--vf-color-button-positive-default']), luminance(values['--vf-color-button-positive-text'])].sort((first, second) => second - first);
    assert.ok((levels[0] + .05) / (levels[1] + .05) >= 4.5);
  }
  colourDom.window.close();
});

test('server field errors associate controls with messages and clear safely', () => {
  const dom = new JSDOM(readFileSync(new URL('../web/index.html', import.meta.url), 'utf8'), { runScripts: 'outside-only' });
  const script = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  dom.window.eval(`${script}\nwindow.errorTest = { showFormError, clearFormError };`);
  const input = dom.window.document.getElementById('user-email');
  const error = dom.window.document.getElementById('user-error');
  input.setAttribute('aria-describedby', 'existing-hint');
  dom.window.errorTest.showFormError(error, { payload: { error: { fields: [{ field: 'email', message: 'invalid email' }] } } });
  assert.equal(input.getAttribute('aria-invalid'), 'true');
  assert.equal(input.getAttribute('aria-describedby'), 'existing-hint user-error');
  dom.window.errorTest.clearFormError(error);
  assert.equal(input.hasAttribute('aria-invalid'), false);
  assert.equal(input.getAttribute('aria-describedby'), 'existing-hint');
  dom.window.close();
});