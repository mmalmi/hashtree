import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
export const sha = b => createHash('sha256').update(b).digest('hex');
export async function loadPayloads(plan) {
  assert.equal(plan.site, 'https://chat.iris.to');
  const rawManifest = await readFile(plan.distManifest);
  assert.equal(sha(rawManifest), plan.distManifestSha256);
  const manifest = JSON.parse(rawManifest); assert.equal(manifest.source, plan.source);
  const files = new Map(), raw = new Map();
  for (const item of manifest.files) {
    assert(!files.has(item.path) && !path.isAbsolute(item.path) && !item.path.split('/').includes('..'));
    const b = await readFile(path.join(plan.sourceRoot, 'dist', item.path));
    assert.equal(b.length, item.bytes); assert.equal(sha(b), item.sha256);
    files.set(item.path, item); raw.set(item.path, b);
  }
  const workerFile = path.join(plan.sourceRoot, 'scripts/static-assets-worker.mjs');
  assert.equal(sha(await readFile(workerFile)), plan.sourceFiles['scripts/static-assets-worker.mjs']);
  const {default: worker} = await import(pathToFileURL(workerFile));
  class ExactHeadInjection {
    on(selector, handler) { assert.equal(selector, 'head'); this.handler = handler; return this; }
    async transform(response) {
      let insert;
      this.handler.element({prepend(text, options) {
        assert.equal(text, '<base href="/">'); assert.equal(options.html, true); insert = text;
      }});
      const text = await response.text(); assert.equal(text.split('<head>').length, 2);
      return new Response(text.replace('<head>', '<head>' + insert), {status: response.status, headers: response.headers});
    }
  }
  globalThis.HTMLRewriter = ExactHeadInjection;
  async function expected(url, item) {
    if (!item.path.endsWith('.html')) return raw.get(item.path);
    const response = await worker.fetch(new Request(url), {ASSETS: {fetch: async request => {
      const pathname = new URL(request.url).pathname;
      const name = pathname === '/__iris_chat_app' ? 'index.html' : pathname.slice(1);
      assert(raw.has(name), 'Unknown worker asset');
      return new Response(raw.get(name), {headers: {'content-type': 'text/html'}});
    }}});
    assert.equal(response.status, 200);
    return Buffer.from(await response.arrayBuffer());
  }
  return {files, raw, expected};
}
