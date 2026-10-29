#!/usr/bin/env node
// 无头 Chrome 截图管理页：先算一个登录 Cookie 注进去，再导航到目标标签截图。
// 用法：node tools/shot-admin.js <secretKeyFile> <baseUrl> <tabQuery> <out.png> [width] [height] [clickSelector]
//
// clickSelector 可选：截图前先点一下这个 CSS 选择器（用来截弹层这种要交互才出现的东西）。
const fs = require('fs');
const crypto = require('crypto');
const { spawn } = require('child_process');

const [keyFile, baseUrl, tabQuery, out, w = '1280', h = '1400', clickSel] = process.argv.slice(2);
const secret = fs.readFileSync(keyFile, 'utf8').trim();
const nonce = crypto.randomBytes(16).toString('hex');
const payload = `a0.${nonce}`;
const sig = crypto.createHmac('sha256', secret).update(payload).digest('hex');
const cookieValue = `${payload}.${sig}`;

const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PORT = 9333 + Math.floor(Math.random() * 400);
const profile = fs.mkdtempSync('/tmp/bks-chrome-');

const chrome = spawn(CHROME, [
  '--headless=new', '--disable-gpu', '--no-sandbox', '--hide-scrollbars',
  '--force-prefers-reduced-motion', '--run-all-compositor-stages-before-draw',
  `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`,
  `--window-size=${w},${h}`, 'about:blank',
], { stdio: 'ignore' });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function json(path) {
  const res = await fetch(`http://127.0.0.1:${PORT}${path}`);
  return res.json();
}

(async () => {
  let target = null;
  for (let i = 0; i < 60 && !target; i++) {
    await sleep(250);
    try {
      const list = await json('/json/list');
      target = list.find((t) => t.type === 'page');
    } catch { /* 还没起来 */ }
  }
  if (!target) { console.error('Chrome 没起来'); process.exit(1); }

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  let id = 0;
  const waiting = new Map();
  const send = (method, params = {}) => new Promise((resolve) => {
    const mid = ++id;
    waiting.set(mid, resolve);
    ws.send(JSON.stringify({ id: mid, method, params }));
  });
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg.result); waiting.delete(msg.id); }
  };
  await new Promise((r) => { ws.onopen = r; });

  await send('Page.enable');
  await send('Network.enable');
  const url = new URL(baseUrl);
  await send('Network.setCookie', {
    name: 'bks_admin', value: cookieValue, domain: url.hostname, path: '/',
  });
  await send('Page.navigate', { url: `${baseUrl}${tabQuery}` });
  await sleep(2500);
  if (clickSel) {
    // 点一下再截：弹层这类要交互才出现的东西
    await send('Runtime.evaluate', {
      expression: `(function(){var el=document.querySelector(${JSON.stringify(clickSel)});
        if(!el) return 'NOT_FOUND'; el.click(); return 'OK';})()`,
      awaitPromise: false,
    });
    await sleep(1800);
  }
  const shot = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true });
  fs.writeFileSync(out, Buffer.from(shot.data, 'base64'));
  console.log(`已截图 ${out}`);
  ws.close();
  chrome.kill();
  try { fs.rmSync(profile, { recursive: true, force: true, maxRetries: 3 }); } catch { /* Chrome 可能还占着文件 */ }
  process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
