#!/usr/bin/env node
// 网格编辑的 CDP 实测：控件有没有建出来、改值失焦后有没有真的存进库。
// 用法：node tools/grid-cdp-test.js <secretKeyFile> <baseUrl>
const fs = require('fs');
const crypto = require('crypto');
const { spawn } = require('child_process');

const [keyFile, baseUrl] = process.argv.slice(2);
const secret = fs.readFileSync(keyFile, 'utf8').trim();
const nonce = crypto.randomBytes(16).toString('hex');
const payload = `a0.${nonce}`;
const sig = crypto.createHmac('sha256', secret).update(payload).digest('hex');
const cookieValue = `${payload}.${sig}`;

const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PORT = 9500 + Math.floor(Math.random() * 400);
const profile = fs.mkdtempSync('/tmp/bks-chrome-');

const chrome = spawn(CHROME, [
  '--headless=new', '--disable-gpu', '--no-sandbox', '--hide-scrollbars',
  '--force-prefers-reduced-motion', '--run-all-compositor-stages-before-draw',
  `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`,
  '--window-size=1280,1400', 'about:blank',
], { stdio: 'ignore' });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  let target = null;
  for (let i = 0; i < 60 && !target; i++) {
    await sleep(250);
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
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
  await send('Runtime.enable');
  await send('Network.enable');
  await send('Network.setCookie', {
    name: 'bks_admin', value: cookieValue, domain: '127.0.0.1', path: '/',
  });

  const fails = [];
  const check = (name, ok, detail) => {
    console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  → ' + detail : ''}`);
    if (!ok) fails.push(name);
  };
  const evaluate = async (expr) => {
    const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    return r.result && r.result.value;
  };

  await send('Page.navigate', { url: `${baseUrl}/admin?tab=register&sub=preview` });
  await sleep(1800);

  /* 1. 控件有没有按字段类型建出来 */
  const types = await evaluate(`(function(){
    var out = {};
    var els = document.querySelectorAll('.ginput');
    out.total = els.length;
    out.byType = {};
    for (var i = 0; i < els.length; i++) {
      var k = els[i].tagName === 'SELECT' ? 'select' : els[i].type;
      out.byType[k] = (out.byType[k] || 0) + 1;
    }
    // 下拉那一列的选项
    var sel = document.querySelector('tr[data-sub="1"] .ginput');
    var sels = document.querySelectorAll('tr[data-sub="1"] select');
    if (sels.length) {
      out.selectOpts = [];
      for (var j = 0; j < sels[0].options.length; j++) out.selectOpts.push(sels[0].options[j].value);
    }
    // 剩下的 td 不该再有文本（被控件替换掉了）
    out.leftoverText = 0;
    var tds = document.querySelectorAll('td.gc[data-key]');
    for (var k = 0; k < tds.length; k++) {
      if (tds[k].textContent.trim() && !tds[k].querySelector('.ginput')) out.leftoverText++;
    }
    return out;
  })()`);
  check('单元格换成了控件', types.total >= 10, `共 ${types.total} 个：${JSON.stringify(types.byType)}`);
  const selVal = await evaluate(`(function(){
    var s = document.querySelectorAll('tr[data-sub="1"] select');
    return s.length ? s[0].value : null;
  })()`);
  check('下拉列渲染成 select 且选中当前值',
    types.selectOpts && types.selectOpts.join(',') === '普通,紧急,特急' && selVal === '特急',
    `选项=${types.selectOpts ? types.selectOpts.join(',') : '无'} 选中=${selVal}`);
  check('没有残留没换掉的文本格', types.leftoverText === 0, `残留 ${types.leftoverText} 个`);

  /* 2. 改一个值 → 触发 change（等于失焦）→ 看脏标记和保存结果 */
  // 先读原值，新值必须跟它不一样（上一轮跑完留在库里的值可能正好等于写死的那个）
  const before = await evaluate(`(function(){
    var el = document.querySelector('tr[data-sub="1"] .ginput[data-key="need"]');
    return { value: el.value, orig: el.getAttribute('data-orig') };
  })()`);
  const NEWVAL = before.orig === 'CDP 改过的客户需求 88888'
    ? 'CDP 改过的客户需求 99999'
    : 'CDP 改过的客户需求 88888';
  await evaluate(`(function(){
    var el = document.querySelector('tr[data-sub="1"] .ginput[data-key="need"]');
    el.value = ${JSON.stringify(NEWVAL)};
    el.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  const dirty = await evaluate(`(function(){
    var tr = document.querySelector('tr[data-sub="1"]');
    return { dotHidden: tr.querySelector('.dot').hidden, st: tr.querySelector('.gst').textContent };
  })()`);
  check('改值后立刻亮未存标记', dirty.dotHidden === false && dirty.dotHidden !== undefined,
    `dot.hidden=${dirty.dotHidden} 状态="${dirty.st}"`);

  await sleep(1500);
  const after = await evaluate(`(function(){
    var tr = document.querySelector('tr[data-sub="1"]');
    var card = tr.closest('.gridcard');
    return {
      dotHidden: tr.querySelector('.dot').hidden,
      st: tr.querySelector('.gst').textContent,
      saved: card.querySelector('[data-role="saved"]').textContent,
      savedHidden: card.querySelector('[data-role="saved"]').hidden,
      orig: tr.querySelector('.ginput[data-key="need"]').getAttribute('data-orig'),
    };
  })()`);
  check('保存成功后未存标记消失', after.dotHidden === true, `状态="${after.st}"`);
  check('保存成功后状态列显示时间', /✓/.test(after.st), after.st);
  check('底部出现已保存提示', after.savedHidden === false && /已保存/.test(after.saved), after.saved);

  /* 3. 值没变就别请求（拿回原值再 change 一次，看 saved 会不会被刷新） */
  const savedBefore = await evaluate(`(function(){
    var tr = document.querySelector('tr[data-sub="1"]');
    return tr.closest('.gridcard').querySelector('[data-role="saved"]').textContent;
  })()`);
  await evaluate(`(function(){
    var el = document.querySelector('tr[data-sub="1"] .ginput[data-key="need"]');
    el.value = el.getAttribute('data-orig');
    el.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  await sleep(600);
  const noop = await evaluate(`(function(){
    var tr = document.querySelector('tr[data-sub="1"]');
    return tr.closest('.gridcard').querySelector('[data-role="saved"]').textContent;
  })()`);
  check('值没变不重复保存', noop === savedBefore, `还是「${noop}」`);

  /* 4. 把值改回去（收尾，别把测试数据留在库里） */
  await evaluate(`(function(){
    var el = document.querySelector('tr[data-sub="1"] .ginput[data-key="need"]');
    el.value = ${JSON.stringify(before.orig)};
    el.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  await sleep(1200);

  /* 5. 必填被清空要被拒，并且行内提示错误 */
  const req = await evaluate(`(function(){
    var tr = document.querySelector('tr[data-sub="2"]');
    var el = tr.querySelector('.ginput[data-key="need"]');
    el.value = '';
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return 'sent';
  })()`);
  await sleep(1500);
  const rej = await evaluate(`(function(){
    var tr = document.querySelector('tr[data-sub="2"]');
    var card = tr.closest('.gridcard');
    return {
      st: tr.querySelector('.gst').textContent,
      err: card.querySelector('[data-role="err"]').textContent,
      errHidden: card.querySelector('[data-role="err"]').hidden,
    };
  })()`);
  check('清空必填被拒', /没存上/.test(rej.st), `状态="${rej.st}"`);
  check('拒的时候显示人话错误', rej.errHidden === false && /客户需求/.test(rej.err), rej.err);

  /* 6. 下拉改了要能存上（换紧急程度） */
  await evaluate(`(function(){
    var tr = document.querySelector('tr[data-sub="1"]');
    var s = tr.querySelector('select');
    s.value = '特急';
    s.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  await sleep(1500);
  const sel = await evaluate(`(function(){
    var tr = document.querySelector('tr[data-sub="1"]');
    return tr.querySelector('.gst').textContent;
  })()`);
  check('下拉改动能存上', /✓/.test(sel), sel);

  /* 7. 「改」链接有 JS 时打开弹层（不整页跳） */
  await send('Runtime.evaluate', {
    expression: `document.querySelector('tr[data-sub="1"] a.rec-open').click()`,
  });
  await sleep(1500);
  const sheet = await evaluate(`(function(){
    var s = document.getElementById('sheet');
    return { hidden: s.hidden, hasForm: !!document.querySelector('#sheet-body form'), title: document.getElementById('sheet-title').textContent };
  })()`);
  check('「改」链接打开编辑弹层', sheet.hidden === false && sheet.hasForm, `标题="${sheet.title}"`);

  console.log('');
  if (fails.length) { console.error('失败 ' + fails.length + ' 项：' + fails.join(' / ')); process.exit(1); }
  console.log('全部通过');
  ws.close();
  chrome.kill();
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });
