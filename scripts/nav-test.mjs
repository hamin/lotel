// In-place navigation test (NAV_JS in crates/lotel/src/ui.rs), run in a real
// Chrome over the DevTools protocol against a running lotel:
//
//   just nav-test                      # against http://127.0.0.1:8080
//   node scripts/nav-test.mjs http://127.0.0.1:18096
//
// Needs Node 22+ (built-in WebSocket) and Chrome (CHROME=… to point at it).
// It only reads pages; the theme click lands in a throwaway browser profile.
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const BASE = process.argv[2] || 'http://127.0.0.1:8080';
const CHROME = process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PORT = 9333;
const prof = mkdtempSync(join(tmpdir(), 'navtest-'));
const chrome = spawn(CHROME, ['--headless', '--disable-gpu', `--remote-debugging-port=${PORT}`,
  `--user-data-dir=${prof}`, '--window-size=1280,900', 'about:blank'], { stdio: 'ignore' });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let fails = 0;
const check = (ok, label, detail = '') => { console.log((ok ? 'PASS ' : 'FAIL ') + label + (detail ? ` — ${detail}` : '')); if (!ok) fails++; };

let ws, id = 0; const pending = new Map(); const errors = []; const requests = [];
async function connect() {
  for (let i = 0; i < 50; i++) {
    try { const r = await fetch(`http://127.0.0.1:${PORT}/json/list`); const l = await r.json(); const p = l.find((t) => t.type === 'page'); if (p) return p.webSocketDebuggerUrl; } catch {}
    await sleep(200);
  }
  throw new Error('chrome did not start');
}
function send(method, params = {}) {
  return new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });
}
async function ev(expr) {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + ' ' + (r.exceptionDetails.exception?.description || ''));
  return r.result.value;
}
async function until(expr, ms = 8000) {
  const t = Date.now();
  while (Date.now() - t < ms) { try { if (await ev(expr)) return true; } catch {} await sleep(100); }
  return false;
}

ws = new WebSocket(await connect());
await new Promise((r) => (ws.onopen = r));
ws.onmessage = (m) => {
  const d = JSON.parse(m.data);
  if (d.id && pending.has(d.id)) { const p = pending.get(d.id); pending.delete(d.id); d.error ? p.rej(new Error(d.error.message)) : p.res(d.result); }
  if (d.method === 'Runtime.exceptionThrown') errors.push(d.params.exceptionDetails.exception?.description || d.params.exceptionDetails.text);
  if (d.method === 'Network.requestWillBeSent') requests.push({ t: Date.now(), url: d.params.request.url, type: d.params.type });
};
await send('Runtime.enable'); await send('Page.enable'); await send('Network.enable');

async function load(path) { await send('Page.navigate', { url: BASE + path }); await until(`document.readyState==='complete' && !!window.LotelNav`); await ev('window.__mark=1'); }
const mark = () => ev('window.__mark===1');
const settled = () => until(`!document.documentElement.hasAttribute('data-nav')`, 10000);

// 1. nav link: in place, nav state follows
await load('/');
await ev(`document.querySelector('nav a[href="/logs"]').click()`);
check(await until(`location.pathname==='/logs' && document.querySelector('h1') && /Logs/.test(document.querySelector('h1').textContent)`), 'nav link swaps to /logs');
check(await mark(), 'no full reload on nav link');
check(await ev(`document.querySelector('nav a[aria-current="page"]').getAttribute('href')`) === '/logs', 'nav highlight follows');
check(await settled(), 'loading state clears');
check(await ev(`document.title.length>0`), 'title kept');

// 2. GET form on the same path keeps the page, updates the URL
const formOk = await ev(`(function(){var f=document.querySelector('form.filters'); if(!f) return false; var q=f.querySelector('input[name="q"]'); if(q) q.value='api_request'; (f.requestSubmit?f.requestSubmit():f.submit()); return true;})()`);
check(formOk, 'logs filter form found');
check(await until(`/q=api_request/.test(location.search)`), 'form submit updates the URL', await ev('location.search'));
check(await mark(), 'no full reload on form submit');

// 3. back/forward
await settled();
await ev('history.back()');
check(await until(`location.pathname==='/logs' && !/q=api_request/.test(location.search)`), 'back restores previous URL', await ev('location.href'));
await ev('history.back()');
check(await until(`location.pathname==='/'`), 'back again to dashboard');
check(await mark(), 'no full reload on back');

// 4. theme link swaps the look without a reload
await settled();
await ev(`document.querySelector('a[href="?pal=riso"]').click()`);
check(await until(`document.documentElement.getAttribute('data-pal')==='riso'`), 'theme link applies the palette');
check(await mark(), 'no full reload on theme change');

// 5. Explore: Run query goes through LotelNav; no duplicate column options
await load('/query?dataset=logs&range=all');
const dup = await ev(`(function(){var s=document.querySelector('#qb select.qcol'); if(!s) return 'no select'; var l=[].map.call(s.options,function(o){return o.textContent}); return l.filter(function(x,i){return l.indexOf(x)!==i}).join(',');})()`);
check(dup === '', 'Explore column options have no duplicates', dup);
await ev(`document.querySelector('.qrun').click()`);
check(await until(`/measures=/.test(location.search)`), 'Run query updates the URL');
check(await mark(), 'no full reload on Run query');
check(await until(`!!document.querySelector('#qb select.qcol')`), 'builder re-boots after swap');

// 6. live polling stops once its page is swapped away
await load('/logs?range=all');
await sleep(1500);
await ev(`document.querySelector('nav a[href="/storage"]').click()`);
await until(`location.pathname==='/storage'`);
const t0 = Date.now(); await sleep(12000);
const stray = requests.filter((r) => r.t > t0 + 500 && /\/api\/live\?topic=(logs|tail)/.test(r.url));
check(stray.length === 0, 'logs live poll stops after leaving /logs', `${stray.length} stray polls`);


// 8. every nav page, reached in place: right h1, nothing stuck dim, charts draw
await load('/');
const hrefs = await ev(`[].map.call(document.querySelectorAll('nav a[href]'),function(a){return a.getAttribute('href')})`);
for (const h of hrefs) {
  await ev(`document.querySelector('nav a[href="${h}"]').click()`);
  const ok = await until(`location.pathname===${JSON.stringify(h)} && !document.documentElement.hasAttribute('data-nav')`, 15000);
  const op = await ev(`getComputedStyle(document.querySelector('main')).opacity`);
  check(ok && op === '1', `in-place ${h}`, `opacity ${op}`);
  check(await mark(), `no reload reaching ${h}`);
}
await ev(`document.querySelector('nav a[href="/metrics"]').click()`);
await until(`location.pathname==='/metrics'`);
const firstMetric = await ev(`(function(){var a=document.querySelector('main a[href*="/metrics?"]'); if(a){a.click(); return a.getAttribute('href');} return null;})()`);
if (firstMetric) {
  check(await until(`!!document.querySelector('.uplot, canvas')`, 10000), 'metric chart draws after an in-place swap', firstMetric);
}
const fresh = await (async () => { await send('Page.reload'); await until(`document.readyState==='complete'`); return ev(`document.documentElement.getAttribute('data-pal')`); })();
check(fresh === 'riso', 'look survives a real reload (cookie set by the swapped request)', fresh);

// 7. no JS errors anywhere
check(errors.length === 0, 'no uncaught JS errors', errors.slice(0, 3).join(' | '));

ws.close(); chrome.kill();
console.log(fails ? `navtest: ${fails} failed` : 'navtest: all passed');
process.exit(fails ? 1 : 0);
