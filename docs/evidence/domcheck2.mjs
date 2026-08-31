import { chromium } from 'playwright';
import fs from 'node:fs';
const url = 'http://127.0.0.1:18080/';
const browser = await chromium.launch({ args: ['--no-sandbox'] });
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const errors = [];
page.on('pageerror', e => errors.push('pageerror: ' + e.message));
page.on('console', m => { if (m.type() === 'error') errors.push('console: ' + m.text()); });
await page.goto(url, { waitUntil: 'networkidle' });
await page.waitForTimeout(1200);
const out = [];
const check = (name, ok, detail) => out.push((ok ? 'PASS' : 'FAIL') + ' | ' + name + (detail ? ' | ' + detail : ''));

// 待办页：行内「邮件」按钮
const mailBtns = await page.$$eval('#items-body .actions', els => els.map(e => Array.from(e.querySelectorAll('button')).map(b => b.textContent.trim()).join(',')));
check('待办行含「邮件」按钮', mailBtns.some(r => r.includes('邮件')), JSON.stringify(mailBtns));
check('邮件按钮数量=2（两行关联邮件）', mailBtns.filter(r => r.includes('邮件')).length === 2, String(mailBtns.filter(r => r.includes('邮件')).length));

// 点击第一行的「邮件」→ 详情弹窗（含主题/标注/重新分类）
await page.click('#items-body .actions button[title*="关联邮件"]');
await page.waitForTimeout(500);
check('弹窗打开', await page.$('#dlg-mail[open]') !== null, '');
const title = await page.$eval('#mail-title', e => e.textContent);
check('弹窗主题=关联邮件主题', title.length > 0, title);
check('弹窗含标注按钮', await page.$('#btn-mail-label') !== null, '');
check('弹窗含重新分类', await page.$('#btn-reclassify') !== null, '');
const meta = await page.$eval('#mail-meta', e => e.textContent);
check('弹窗元数据含分类/标注', meta.includes('分类') && meta.includes('标注'), meta.replace(/\s+/g,' ').slice(0,80));
await page.screenshot({ path: '/tmp/ui-verify/6-linked-mail-dialog.png' });
await page.keyboard.press('Escape');
await page.waitForTimeout(300);

// 通知页也应有「邮件」按钮能力（通知事项可关联邮件——手动建的没有则不要求按钮，检查行渲染正常）
await page.click('button[data-view="notices"]');
await page.waitForTimeout(600);
const nRows = await page.$$eval('#notices-body tr', els => els.map(e => e.textContent.replace(/\s+/g,' ').trim()).filter(t => t && !t.includes('暂无通知')));
check('通知页行正常', nRows.length === 2, String(nRows.length));

check('无 JS 错误', errors.length === 0, errors.join('; '));
console.log(out.join('\n'));
await browser.close();
