/* mail2 控制台 —— 原生 JS SPA（无构建），只经 /api/v1 与后端通信 */

const API = '/api/v1';
let CATEGORIES = [];  // [{id,label,create_item,kind,source}]
let ACCOUNTS = [];    // [{id,label,address,...,enabled,is_default}]
let ITEMS = [];       // 当前列表缓存
let EMAILS = [];
let TOKEN = localStorage.getItem('mail2_token') || '';

async function api(path, opts = {}) {
  const headers = { 'Content-Type': 'application/json' };
  if (TOKEN) headers['Authorization'] = 'Bearer ' + TOKEN;
  const resp = await fetch(API + path, { ...opts, headers });
  let body = null;
  try { body = await resp.json(); } catch (_) { /* 非 JSON */ }
  if (resp.status === 401) {
    const t = prompt('需要访问令牌（config.auth_token），请输入：');
    if (t !== null) {
      TOKEN = t.trim();
      localStorage.setItem('mail2_token', TOKEN);
      return api(path, opts);
    }
    throw new Error('未授权');
  }
  if (!resp.ok) {
    throw new Error((body && body.message) || `HTTP ${resp.status}`);
  }
  return body;
}

const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
}[c]));

const catBadge = (id) => {
  const c = CATEGORIES.find((x) => x.id === id);
  return `<span class="badge ${esc(id)}">${esc(c ? c.label : id || '未分类')}</span>`;
};
const statusBadge = (s) => `<span class="badge ${esc(s)}">${esc({ active: '进行中', completed: '已完成', silent: '已静默', expired: '已过期' }[s] || s)}</span>`;
const fmt = (s) => (s ? new Date(s).toLocaleString('zh-CN', { hour12: false }) : '—');

// 邮件标注预设（与后端存储一致；自由文本也可）
const LABELS = {
  correct: '分类正确', wrong_category: '分类错误', should_todo: '应建待办',
  should_not_todo: '不应建待办', deadline_missing: '漏截止时间', other: '其他',
};
const labelBadge = (l, note) =>
  l ? `<span class="badge label ${esc(l)}" title="${esc(note || '')}">${esc(LABELS[l] || l)}</span>` : '';

// ---------------- 初始化 ----------------
async function loadConfig() {
  try {
    const r = await api('/config');
    CATEGORIES = (r.data && r.data.categories) || [];
    ACCOUNTS = (r.data && r.data.accounts) || [];
    fillCategorySelects();
    fillLabelSelect();
    fillAccountSelect();
    document.getElementById('health').textContent = '✅ 服务正常';
    document.getElementById('health').className = 'health ok';
  } catch (e) {
    document.getElementById('health').textContent = '⚠️ 无法连接后端: ' + e.message;
    document.getElementById('health').className = 'health err';
  }
}

function fillCategorySelects() {
  const items = CATEGORIES.map((c) => `<option value="${esc(c.id)}">${esc(c.label)}</option>`).join('');
  document.getElementById('flt-cat').innerHTML = '<option value="">全部分类</option>' + items;
  document.getElementById('flt-notice-cat').innerHTML = '<option value="">全部分类</option>' + items;
  document.getElementById('flt-mail-cat').innerHTML = '<option value="">全部分类</option>' + items;
  document.getElementById('f-category').innerHTML = items;
  document.getElementById('reclass-cat').innerHTML = items;
  document.getElementById('batch-mail-cat').innerHTML = '<option value="">选择目标分类…</option>' + items;
}

function fillLabelSelect() {
  const items = Object.entries(LABELS)
    .map(([v, t]) => `<option value="${esc(v)}">${esc(t)}</option>`).join('');
  document.getElementById('flt-mail-label').innerHTML =
    '<option value="">全部标注</option><option value="__none__">未标注</option>' + items;
}

function fillAccountSelect() {
  const items = ACCOUNTS.map((a) =>
    `<option value="${a.id}">${esc(a.label || a.address)}${a.is_default ? '（默认）' : ''}</option>`).join('');
  document.getElementById('fetch-account').innerHTML = items;
}

// ---------------- 待办 / 通知（两个独立页面） ----------------
let NOTICES = [];

async function loadTodos() {
  const params = new URLSearchParams();
  params.set('kind', 'todo');
  const get = (id) => document.getElementById(id).value;
  if (get('flt-status')) params.set('status', get('flt-status'));
  if (get('flt-cat')) params.set('category', get('flt-cat'));
  if (get('flt-party')) params.set('party', get('flt-party'));
  const q = get('flt-q').trim();
  if (q) params.set('q', q);
  try {
    const r = await api('/items?' + params.toString());
    ITEMS = r.data || [];
    renderTodos();
  } catch (e) {
    document.getElementById('items-body').innerHTML =
      `<tr><td colspan="8" class="empty">加载失败：${esc(e.message)}</td></tr>`;
  }
}

async function loadNotices() {
  const params = new URLSearchParams();
  params.set('kind', 'notification');
  const get = (id) => document.getElementById(id).value;
  if (get('flt-notice-status')) params.set('status', get('flt-notice-status'));
  if (get('flt-notice-cat')) params.set('category', get('flt-notice-cat'));
  const q = get('flt-notice-q').trim();
  if (q) params.set('q', q);
  try {
    const r = await api('/items?' + params.toString());
    NOTICES = r.data || [];
    renderNotices();
  } catch (e) {
    document.getElementById('notices-body').innerHTML =
      `<tr><td colspan="7" class="empty">加载失败：${esc(e.message)}</td></tr>`;
  }
}

async function loadParties() {
  try {
    const r = await api('/meta/parties');
    const cur = document.getElementById('flt-party').value;
    document.getElementById('flt-party').innerHTML = '<option value="">全部单位/联系人</option>' +
      (r.data || []).map((p) => `<option value="${esc(p)}">${esc(p)}</option>`).join('');
    document.getElementById('flt-party').value = cur;
  } catch (_) { /* 忽略 */ }
}

function renderTodos() {
  const body = document.getElementById('items-body');
  if (!ITEMS.length) {
    body.innerHTML = '<tr><td colspan="8" class="empty">暂无待办</td></tr>';
    return;
  }
  body.innerHTML = ITEMS.map((it) => `
    <tr>
      <td><input type="checkbox" class="row-chk" value="${it.id}"></td>
      <td><strong>${esc(it.event || it.title)}</strong>
          ${it.needs_review ? ' <span class="badge pending">待补截止时间</span>' : ''}
          <div class="small muted">${esc(it.title)}</div>
          ${it.notes ? `<div class="small muted">${esc(it.notes.slice(0, 60))}</div>` : ''}</td>
      <td>${esc(it.party || '—')}</td>
      <td>${catBadge(it.category)}</td>
      <td>${fmt(it.deadline)}</td>
      <td class="small muted">${fmt(it.remind_at)}</td>
      <td>${statusBadge(it.status)}</td>
      <td><div class="actions">
        ${it.status === 'active' ? `<button class="btn small" onclick="act(${it.id},'complete')">完成</button>
        <button class="btn small" onclick="act(${it.id},'silent')">静默</button>
        <button class="btn small" onclick="act(${it.id},'remind')">提醒</button>` : ''}
        ${it.status === 'expired' ? `<button class="btn small" onclick="act(${it.id},'activate')">恢复</button>` : ''}
        ${it.source_email_id ? `<button class="btn small" onclick="openLinkedMail(${it.source_email_id})" title="查看关联邮件（可对照与标注）">邮件</button>` : ''}
        <button class="btn small" onclick="editItem(${it.id})">编辑</button>
        <button class="btn small danger" onclick="delItem(${it.id})">删除</button>
      </div></td>
    </tr>`).join('');
}

function renderNotices() {
  const body = document.getElementById('notices-body');
  if (!NOTICES.length) {
    body.innerHTML = '<tr><td colspan="7" class="empty">暂无通知事项（通知类邮件不自动建项，可在「邮件」页查看）</td></tr>';
    return;
  }
  body.innerHTML = NOTICES.map((it) => `
    <tr>
      <td><input type="checkbox" class="row-chk" value="${it.id}"></td>
      <td><strong>${esc(it.event || it.title)}</strong>
          <div class="small muted">${esc(it.title)}</div>
          ${it.notes ? `<div class="small muted">${esc(it.notes.slice(0, 60))}</div>` : ''}</td>
      <td>${esc(it.party || '—')}</td>
      <td>${catBadge(it.category)}</td>
      <td class="small muted">${fmt(it.created_at)}</td>
      <td>${statusBadge(it.status)}</td>
      <td><div class="actions">
        ${it.status === 'active' ? `<button class="btn small" onclick="act(${it.id},'complete')">完成</button>
        <button class="btn small" onclick="act(${it.id},'silent')">静默</button>` : ''}
        ${it.status === 'expired' ? `<button class="btn small" onclick="act(${it.id},'activate')">恢复</button>` : ''}
        ${it.source_email_id ? `<button class="btn small" onclick="openLinkedMail(${it.source_email_id})" title="查看关联邮件（可对照与标注）">邮件</button>` : ''}
        <button class="btn small" onclick="editItem(${it.id})">编辑</button>
        <button class="btn small danger" onclick="delItem(${it.id})">删除</button>
      </div></td>
    </tr>`).join('');
}

async function act(id, op) {
  try {
    if (op === 'remind') {
      const r = await api(`/items/${id}/remind-now`, { method: 'POST' });
      const log = r.data;
      alert(log.status === 'sent'
        ? '提醒已发送：' + log.subject
        : '发送失败：' + (log.error || '未知错误'));
    } else {
      await api(`/items/${id}/${op}`, { method: 'POST' });
    }
    await loadTodos();
    await loadNotices();
  } catch (e) {
    alert('操作失败：' + e.message);
  }
}

async function delItem(id) {
  if (!confirm('确认删除该事项及其发送日志？')) return;
  try {
    await api(`/items/${id}`, { method: 'DELETE' });
    await loadTodos();
    await loadNotices();
  } catch (e) {
    alert('删除失败：' + e.message);
  }
}

function checkedIds(scope) {
  return [...document.querySelectorAll(`#${scope}-body .row-chk:checked`)].map((c) => Number(c.value));
}

async function batchItems(action, useFilters, scope) {
  const msgId = scope === 'notices' ? 'batch-notice-msg' : 'batch-msg';
  let payload;
  if (useFilters) {
    const from = document.getElementById('batch-from').value;
    const to = document.getElementById('batch-to').value;
    if (!from && !to) { alert('请先填写时间区间（或勾选事项）'); return; }
    payload = {
      action,
      filters: {
        created_after: from ? new Date(from).toISOString() : null,
        created_before: to ? new Date(to).toISOString() : null,
      },
    };
  } else {
    const ids = checkedIds(scope);
    if (!ids.length) { alert('请先勾选要处理的事项'); return; }
    if (action === 'delete' && !confirm(`确认删除选中的 ${ids.length} 个事项及其发送日志？`)) return;
    payload = { action, ids };
  }
  try {
    const r = await api('/items/batch', { method: 'POST', body: JSON.stringify(payload) });
    document.getElementById(msgId).textContent =
      `已处理：匹配 ${r.data.matched}，变更 ${r.data.changed}`;
    document.getElementById(msgId).className = 'small ok';
    await loadTodos();
    await loadNotices();
  } catch (e) {
    document.getElementById(msgId).textContent = '失败：' + e.message;
    document.getElementById(msgId).className = 'small err';
  }
}

function editItem(id) {
  const it = ITEMS.find((x) => x.id === id);
  if (!it) return;
  document.getElementById('dlg-title').textContent = '编辑事项 #' + id;
  document.getElementById('f-id').value = id;
  document.getElementById('f-kind').value = it.kind;
  document.getElementById('f-title').value = it.title;
  document.getElementById('f-event').value = it.event;
  document.getElementById('f-party').value = it.party;
  document.getElementById('f-category').value = it.category;
  document.getElementById('f-deadline').value = it.deadline ? toLocalInput(it.deadline) : '';
  document.getElementById('f-notes').value = it.notes || '';
  document.getElementById('dlg-item').showModal();
}

function toLocalInput(rfc3339) {
  const d = new Date(rfc3339);
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

async function saveItem(ev) {
  ev.preventDefault();
  const id = document.getElementById('f-id').value;
  const deadlineLocal = document.getElementById('f-deadline').value;
  const payload = {
    kind: document.getElementById('f-kind').value,
    title: document.getElementById('f-title').value.trim(),
    event: document.getElementById('f-event').value.trim(),
    party: document.getElementById('f-party').value.trim(),
    category: document.getElementById('f-category').value.trim() || 'todo',
    deadline: deadlineLocal ? new Date(deadlineLocal).toISOString() : null,
    notes: document.getElementById('f-notes').value.trim(),
  };
  try {
    if (id) {
      await api(`/items/${id}`, { method: 'PUT', body: JSON.stringify(payload) });
    } else {
      await api('/items', { method: 'POST', body: JSON.stringify(payload) });
    }
    document.getElementById('dlg-item').close();
    await loadTodos();
    await loadNotices();
  } catch (e) {
    alert('保存失败：' + e.message);
  }
}

/// 新建事项：kind 决定默认类型（todo=待办页 / notification=通知页）
function openNewItem(kind) {
  document.getElementById('dlg-title').textContent = kind === 'notification' ? '新建通知' : '新建待办';
  document.getElementById('form-item').reset();
  document.getElementById('f-id').value = '';
  document.getElementById('f-kind').value = kind;
  document.getElementById('f-category').value = kind === 'notification' ? 'notification' : 'todo';
  document.getElementById('dlg-item').showModal();
}

async function addCategory() {
  const label = prompt('新分类名称（如 线上测评）：');
  if (!label) return;
  const id = prompt('分类 id（小写字母/数字/下划线，如 online_assessment）：');
  if (!id) return;
  try {
    await api('/categories', {
      method: 'POST',
      body: JSON.stringify({ id, label, create_item: true, kind: 'todo' }),
    });
    await loadConfig();
    fillCategorySelects();
    document.getElementById('f-category').value = id;
  } catch (e) {
    alert('新增分类失败：' + e.message);
  }
}

async function runCheck() {
  try {
    const r = await api('/checker/run', { method: 'POST' });
    const d = r.data;
    alert(`检查完成：过期标记 ${d.expired}，漏发补偿 ${d.missed_compensated}，发送成功 ${d.reminders_sent}，失败 ${d.reminders_failed}`);
    await loadTodos();
    await loadNotices();
    await loadLogs();
  } catch (e) {
    alert('检查失败：' + e.message);
  }
}

// ---------------- 邮件 ----------------
async function loadEmails() {
  const params = new URLSearchParams();
  const get = (id) => document.getElementById(id).value;
  if (get('flt-mail-cat')) params.set('category', get('flt-mail-cat'));
  if (get('flt-mail-label')) params.set('label', get('flt-mail-label'));
  const q = get('flt-mail-q').trim();
  if (q) params.set('q', q);
  if (get('flt-mail-since')) params.set('since', new Date(get('flt-mail-since')).toISOString());
  if (get('flt-mail-before')) params.set('before', new Date(get('flt-mail-before')).toISOString());
  try {
    const r = await api('/emails?' + params.toString());
    EMAILS = r.data || [];
    renderEmails();
  } catch (e) {
    document.getElementById('emails-body').innerHTML =
      `<tr><td colspan="8" class="empty">加载失败：${esc(e.message)}</td></tr>`;
  }
}

function renderEmails() {
  const body = document.getElementById('emails-body');
  if (!EMAILS.length) {
    body.innerHTML = '<tr><td colspan="8" class="empty">暂无邮件</td></tr>';
    return;
  }
  body.innerHTML = EMAILS.map((m) => `
    <tr>
      <td><input type="checkbox" class="row-chk" value="${m.id}"></td>
      <td>${catBadge(m.category)}</td>
      <td><strong>${esc(m.subject)}</strong></td>
      <td>${esc(m.from_name ? m.from_name + ' <' + m.from_addr + '>' : m.from_addr)}</td>
      <td class="small">${fmt(m.sent_at || '')}</td>
      <td class="small muted">${fmt(m.received_at)}</td>
      <td>${labelBadge(m.user_label, m.user_note)}</td>
      <td><div class="actions">
        <button class="btn small" onclick="showMail(${m.id})">查看</button>
        <button class="btn small" onclick="openLabel(${m.id})">标注</button>
        <button class="btn small danger" onclick="delMail(${m.id})">删除</button>
      </div></td>
    </tr>`).join('');
}

/// 手动拉取邮箱（需求 1）：范围 = 从指定时间后 / 近3天/7天/15天/一个月
async function fetchMail() {
  const range = document.getElementById('fetch-range').value;
  const sinceLocal = document.getElementById('fetch-since').value;
  const payload = { account_id: Number(document.getElementById('fetch-account').value) || null };
  let note = '';
  if (range) {
    payload.range = range;
  } else if (sinceLocal) {
    payload.since = new Date(sinceLocal).toISOString();
  } else {
    // 未选择范围也未填时间：兜底按近 3 天拉取，避免按钮点了无反应
    payload.range = '3d';
    note = '（未选择范围，已按近 3 天拉取）';
  }
  const btn = document.getElementById('btn-fetch-mail');
  btn.disabled = true;
  const msg = document.getElementById('fetch-msg');
  msg.textContent = '拉取处理中…（包含 LLM 分析，可能较慢）';
  msg.className = 'small';
  try {
    const r = await api('/mail/fetch', { method: 'POST', body: JSON.stringify(payload) });
    const d = r.data;
    let text;
    if (d.fetched === 0) {
      text = '拉取完成：邮箱暂无新邮件';
    } else if (d.new_emails === 0) {
      text = `拉取完成：获取 ${d.fetched} 封，均为已处理过的邮件（无新增）`;
    } else {
      text = `拉取完成：获取 ${d.fetched} 封，新邮件 ${d.new_emails}，建事项 ${d.items_created}，Agent 轮次 ${d.tool_rounds}，审批申请 ${d.approvals_created}`;
    }
    msg.textContent = text + note;
    msg.className = 'small ok';
    await loadEmails();
    await loadTodos();
    await loadNotices();
    await loadApprovals();
  } catch (e) {
    msg.textContent = '拉取失败：' + e.message;
    msg.className = 'small err';
  } finally {
    btn.disabled = false;
  }
}

function showMail(id) {
  const m = EMAILS.find((x) => x.id === id);
  if (!m) return;
  renderMailDialog(m);
  document.getElementById('dlg-mail').showModal();
}

/// 填充邮件详情弹窗（含重新分类 + 标注）
function renderMailDialog(m) {
  document.getElementById('mail-title').textContent = m.subject;
  document.getElementById('mail-meta').innerHTML =
    `发件人：${esc(m.from_name)} &lt;${esc(m.from_addr)}&gt; ｜ 发件时间：${fmt(m.sent_at || '')} ｜ 收件入库：${fmt(m.received_at)} ｜ 分类：${catBadge(m.category)} ｜ 标注：${labelBadge(m.user_label, m.user_note) || '<span class="muted small">未标注</span>'}`;
  document.getElementById('reclass-cat').value = m.category || '';
  document.getElementById('reclass-msg').textContent = '';
  document.getElementById('mail-body').textContent = m.body_text;
  document.getElementById('dlg-mail').dataset.id = m.id;
}

/// 从事项列表跳转查看关联邮件（方便对照查看与标注）
async function openLinkedMail(emailId) {
  try {
    const r = await api(`/emails/${emailId}`);
    renderMailDialog(r.data);
    document.getElementById('dlg-mail').showModal();
  } catch (e) {
    alert('关联邮件不存在或已删除：' + e.message);
  }
}

/// 打开标注弹窗（从邮件列表或邮件详情）
function openLabel(id) {
  const m = EMAILS.find((x) => x.id === id);
  if (!m) return;
  document.getElementById('l-email-id').value = id;
  document.getElementById('label-mail-subject').textContent = m.subject;
  document.getElementById('l-label').value = m.user_label || '';
  document.getElementById('l-note').value = m.user_note || '';
  document.getElementById('dlg-label').dataset.from = document.getElementById('dlg-mail').open ? 'mail' : 'list';
  document.getElementById('dlg-label').showModal();
}

async function saveLabel() {
  const id = Number(document.getElementById('l-email-id').value);
  if (!id) return;
  try {
    await api(`/emails/${id}/label`, {
      method: 'PUT',
      body: JSON.stringify({
        label: document.getElementById('l-label').value,
        note: document.getElementById('l-note').value,
      }),
    });
    document.getElementById('dlg-label').close();
    await loadEmails();
  } catch (e) {
    alert('标注保存失败：' + e.message);
  }
}

async function clearLabel() {
  const id = Number(document.getElementById('l-email-id').value);
  if (!id) return;
  if (!confirm('清除该邮件的标注？')) return;
  try {
    await api(`/emails/${id}/label`, { method: 'PUT', body: JSON.stringify({ label: '', note: '' }) });
    document.getElementById('dlg-label').close();
    await loadEmails();
  } catch (e) {
    alert('清除失败：' + e.message);
  }
}

async function reclassify() {
  const id = document.getElementById('dlg-mail').dataset.id;
  const cat = document.getElementById('reclass-cat').value;
  try {
    const r = await api(`/emails/${id}/reclassify`, {
      method: 'POST',
      body: JSON.stringify({ category: cat }),
    });
    document.getElementById('reclass-msg').textContent =
      '已重新分类为 ' + cat + (r.data.item_updated ? '，并更新事项' : '');
    await loadEmails();
    await loadTodos();
    await loadNotices();
  } catch (e) {
    document.getElementById('reclass-msg').textContent = '失败：' + e.message;
  }
}

async function delMail(id) {
  if (!confirm('确认删除该邮件记录？')) return;
  try {
    await api(`/emails/${id}`, { method: 'DELETE' });
    await loadEmails();
  } catch (e) {
    alert('删除失败：' + e.message);
  }
}

async function batchEmails(action) {
  const ids = [...document.querySelectorAll('#emails-body .row-chk:checked')].map((c) => Number(c.value));
  if (!ids.length) { alert('请先勾选邮件'); return; }
  if (action === 'delete' && !confirm(`确认删除选中的 ${ids.length} 封邮件？`)) return;
  const payload = { action, ids };
  if (action === 'reclassify') {
    payload.category = document.getElementById('batch-mail-cat').value;
    if (!payload.category) { alert('请选择目标分类'); return; }
  }
  try {
    const r = await api('/emails/batch', { method: 'POST', body: JSON.stringify(payload) });
    document.getElementById('batch-mail-msg').textContent =
      `已处理：匹配 ${r.data.matched}，变更 ${r.data.changed}`;
    document.getElementById('batch-mail-msg').className = 'small ok';
    await loadEmails();
    await loadTodos();
    await loadNotices();
  } catch (e) {
    document.getElementById('batch-mail-msg').textContent = '失败：' + e.message;
    document.getElementById('batch-mail-msg').className = 'small err';
  }
}

// ---------------- 邮箱账户 ----------------
async function loadAccounts() {
  try {
    const r = await api('/accounts');
    ACCOUNTS = r.data || [];
    renderAccounts();
    fillAccountSelect();
  } catch (e) {
    document.getElementById('accounts-body').innerHTML =
      `<tr><td colspan="7" class="empty">加载失败：${esc(e.message)}</td></tr>`;
  }
}

function renderAccounts() {
  const body = document.getElementById('accounts-body');
  if (!ACCOUNTS.length) {
    body.innerHTML = '<tr><td colspan="7" class="empty">暂无账户，请点击“添加邮箱”</td></tr>';
    return;
  }
  body.innerHTML = ACCOUNTS.map((a) => `
    <tr>
      <td>${esc(a.label || '—')}</td>
      <td>${esc(a.address)}</td>
      <td class="small muted">${esc(a.imap_host)}:${a.imap_port}</td>
      <td class="small muted">${esc(a.smtp_host)}:${a.smtp_port}</td>
      <td>${a.enabled ? '<span class="badge sent">启用</span>' : '<span class="badge silent">停用</span>'}</td>
      <td>${a.is_default ? '<span class="badge active">默认</span>' : ''}</td>
      <td><div class="actions">
        <button class="btn small" onclick="testAccount(${a.id})">测试连接</button>
        ${a.is_default ? '' : `<button class="btn small" onclick="setDefaultAccount(${a.id})">设为默认</button>`}
        <button class="btn small" onclick="editAccount(${a.id})">编辑</button>
        <button class="btn small danger" onclick="delAccount(${a.id})">删除</button>
      </div></td>
    </tr>`).join('');
}

function openAccount(id) {
  document.getElementById('form-account').reset();
  document.getElementById('acct-test-msg').textContent = '';
  document.getElementById('a-enabled').checked = true;
  if (id) {
    const a = ACCOUNTS.find((x) => x.id === id);
    if (!a) return;
    document.getElementById('acct-title').textContent = '编辑邮箱 #' + id;
    document.getElementById('a-id').value = id;
    document.getElementById('a-label').value = a.label || '';
    document.getElementById('a-address').value = a.address;
    document.getElementById('a-imap-host').value = a.imap_host;
    document.getElementById('a-imap-port').value = a.imap_port;
    document.getElementById('a-imap-user').value = a.imap_user;
    document.getElementById('a-imap-password').value = a.imap_password === '***' ? '' : '';
    document.getElementById('a-smtp-host').value = a.smtp_host;
    document.getElementById('a-smtp-port').value = a.smtp_port;
    document.getElementById('a-smtp-user').value = a.smtp_user;
    document.getElementById('a-smtp-password').value = a.smtp_password === '***' ? '' : '';
    document.getElementById('a-imap-tls-insecure').checked = a.imap_tls_insecure;
    document.getElementById('a-smtp-tls-insecure').checked = a.smtp_tls_insecure;
    document.getElementById('a-enabled').checked = a.enabled;
    document.getElementById('a-reminder-to').value = a.reminder_to || '';
  } else {
    document.getElementById('acct-title').textContent = '添加邮箱';
    document.getElementById('a-id').value = '';
  }
  document.getElementById('dlg-account').showModal();
}

function accountPayload() {
  const v = (id) => document.getElementById(id).value.trim();
  return {
    label: v('a-label'),
    address: v('a-address'),
    imap_host: v('a-imap-host'),
    imap_port: Number(v('a-imap-port')) || null,
    imap_user: v('a-imap-user'),
    imap_password: v('a-imap-password') || null,
    imap_tls_insecure: document.getElementById('a-imap-tls-insecure').checked,
    smtp_host: v('a-smtp-host'),
    smtp_port: Number(v('a-smtp-port')) || null,
    smtp_user: v('a-smtp-user'),
    smtp_password: v('a-smtp-password') || null,
    smtp_tls_insecure: document.getElementById('a-smtp-tls-insecure').checked,
    enabled: document.getElementById('a-enabled').checked,
    reminder_to: v('a-reminder-to'),
  };
}

async function saveAccount(ev) {
  ev.preventDefault();
  const id = document.getElementById('a-id').value;
  const payload = accountPayload();
  // 密码留空：新建时报错，编辑时表示保持不变
  if (!id && (!payload.imap_password || !payload.smtp_password)) {
    alert('新建账户需要填写 IMAP/SMTP 密码');
    return;
  }
  try {
    if (id) {
      await api(`/accounts/${id}`, { method: 'PUT', body: JSON.stringify(payload) });
    } else {
      await api('/accounts', { method: 'POST', body: JSON.stringify(payload) });
    }
    document.getElementById('dlg-account').close();
    await loadAccounts();
  } catch (e) {
    alert('保存失败：' + e.message);
  }
}

async function testAccount(id) {
  const isNew = !document.getElementById('a-id').value;
  const msg = document.getElementById('acct-test-msg');
  msg.textContent = '测试中…';
  msg.className = 'small';
  try {
    let r;
    if (isNew) {
      // 未保存的账户：走无 id 测试端点
      r = await api('/accounts/test', { method: 'POST', body: JSON.stringify(accountPayload()) });
    } else {
      const p = accountPayload();
      // 密码留空 = 用已存密码
      r = await api(`/accounts/${id}/test`, { method: 'POST', body: JSON.stringify(p) });
    }
    msg.textContent = r.data.message;
    msg.className = 'small ' + (r.data.ok ? 'ok' : 'err');
  } catch (e) {
    msg.textContent = '测试失败：' + e.message;
    msg.className = 'small err';
  }
}

async function delAccount(id) {
  if (!confirm('确认删除该邮箱账户？（邮件数据保留）')) return;
  try {
    await api(`/accounts/${id}`, { method: 'DELETE' });
    await loadAccounts();
  } catch (e) {
    alert('删除失败：' + e.message);
  }
}

async function setDefaultAccount(id) {
  try {
    await api(`/accounts/${id}/default`, { method: 'POST' });
    await loadAccounts();
  } catch (e) {
    alert('设置默认失败：' + e.message);
  }
}

// ---------------- 审批 ----------------
async function loadApprovals() {
  const status = document.getElementById('flt-approval-status').value;
  try {
    const r = await api('/approvals?status=' + encodeURIComponent(status));
    const rows = r.data || [];
    const body = document.getElementById('approvals-body');
    if (!rows.length) {
      body.innerHTML = '<tr><td colspan="7" class="empty">暂无审批单</td></tr>';
      return;
    }
    const toolText = {
      update_item: '修改事项', set_item_status: '改状态', delete_item: '删除事项',
      update_email_category: '重分类邮件', delete_email: '删除邮件',
    };
    const statusText = { pending: '待处理', approved: '已批准', rejected: '已拒绝' };
    body.innerHTML = rows.map((a) => `
      <tr>
        <td>#${a.id}</td>
        <td>${esc(toolText[a.tool_name] || a.tool_name)}</td>
        <td>${esc(a.summary)}</td>
        <td class="small">${a.source_email_id ? `邮件 #${a.source_email_id}` : '—'}</td>
        <td class="small muted">${fmt(a.created_at)}</td>
        <td><span class="badge ${esc(a.status)}">${esc(statusText[a.status] || a.status)}</span></td>
        <td><div class="actions">
          ${a.status === 'pending' ? `
            <button class="btn small" onclick="decideApproval(${a.id},'approve')">批准</button>
            <button class="btn small danger" onclick="decideApproval(${a.id},'reject')">拒绝</button>` : ''}
        </div></td>
      </tr>`).join('');
  } catch (e) {
    document.getElementById('approvals-body').innerHTML =
      `<tr><td colspan="7" class="empty">加载失败：${esc(e.message)}</td></tr>`;
  }
}

async function decideApproval(id, op) {
  try {
    const r = await api(`/approvals/${id}/${op}`, { method: 'POST' });
    if (op === 'approve') {
      alert(r.data.approved ? `已批准并执行：${r.data.tool}` : '执行失败（已拒绝）：' + (r.data.reason || ''));
    }
    await loadApprovals();
    await loadTodos();
    await loadNotices();
    await loadEmails();
  } catch (e) {
    alert('操作失败：' + e.message);
  }
}

// ---------------- 日志 ----------------
async function loadLogs() {
  const status = document.getElementById('flt-log-status').value;
  const params = new URLSearchParams();
  if (status) params.set('status', status);
  try {
    const r = await api('/logs?' + params.toString());
    const logs = r.data || [];
    const body = document.getElementById('logs-body');
    if (!logs.length) {
      body.innerHTML = '<tr><td colspan="9" class="empty">暂无发送记录</td></tr>';
      return;
    }
    const statusText = { pending: '待发送', sent: '已发送', failed: '失败' };
    body.innerHTML = logs.map((l) => `
      <tr>
        <td>#${l.item_id}</td>
        <td>${esc(l.subject)}</td>
        <td class="small">${esc({ reminder: '定时提醒', retry: '重试', manual: '手动' }[l.kind] || l.kind)}</td>
        <td><span class="badge ${esc(l.status)}">${esc(statusText[l.status] || l.status)}</span></td>
        <td>${l.attempt}</td>
        <td class="small muted">${fmt(l.scheduled_at)}</td>
        <td class="small muted">${fmt(l.sent_at)}</td>
        <td class="small muted">${fmt(l.next_retry_at)}</td>
        <td class="small" style="color:var(--red)">${esc((l.error || '').slice(0, 120))}</td>
      </tr>`).join('');
  } catch (e) {
    document.getElementById('logs-body').innerHTML =
      `<tr><td colspan="9" class="empty">加载失败：${esc(e.message)}</td></tr>`;
  }
}

// ---------------- 事件绑定 ----------------
document.addEventListener('DOMContentLoaded', () => {
  document.querySelectorAll('.tab').forEach((t) => {
    t.addEventListener('click', () => {
      document.querySelectorAll('.tab').forEach((x) => x.classList.remove('active'));
      document.querySelectorAll('.view').forEach((x) => x.classList.remove('active'));
      t.classList.add('active');
      document.getElementById('view-' + t.dataset.view).classList.add('active');
    });
  });
  // 待办
  document.getElementById('btn-refresh-items').addEventListener('click', loadTodos);
  document.getElementById('btn-new-item').addEventListener('click', () => openNewItem('todo'));
  document.getElementById('btn-dlg-cancel').addEventListener('click', () => document.getElementById('dlg-item').close());
  document.getElementById('form-item').addEventListener('submit', saveItem);
  document.getElementById('btn-add-cat').addEventListener('click', addCategory);
  document.getElementById('btn-run-check').addEventListener('click', runCheck);
  document.getElementById('items-check-all').addEventListener('change', (e) => {
    document.querySelectorAll('#items-body .row-chk').forEach((c) => { c.checked = e.target.checked; });
  });
  // 通知
  document.getElementById('btn-refresh-notices').addEventListener('click', loadNotices);
  document.getElementById('btn-new-notice').addEventListener('click', () => openNewItem('notification'));
  document.getElementById('notices-check-all').addEventListener('change', (e) => {
    document.querySelectorAll('#notices-body .row-chk').forEach((c) => { c.checked = e.target.checked; });
  });

  // emails
  document.getElementById('btn-refresh-emails').addEventListener('click', loadEmails);
  document.getElementById('btn-fetch-mail').addEventListener('click', fetchMail);
  document.getElementById('fetch-range').addEventListener('change', (e) => {
    document.getElementById('fetch-since').disabled = e.target.value !== '';
  });
  document.getElementById('emails-check-all').addEventListener('change', (e) => {
    document.querySelectorAll('#emails-body .row-chk').forEach((c) => { c.checked = e.target.checked; });
  });
  document.getElementById('btn-mail-close').addEventListener('click', () => document.getElementById('dlg-mail').close());
  document.getElementById('btn-reclassify').addEventListener('click', reclassify);
  document.getElementById('btn-mail-label').addEventListener('click', () => {
    const id = document.getElementById('dlg-mail').dataset.id;
    if (id) openLabel(Number(id));
  });
  document.getElementById('btn-label-cancel').addEventListener('click', () => document.getElementById('dlg-label').close());
  document.getElementById('btn-label-save').addEventListener('click', saveLabel);
  document.getElementById('btn-label-clear').addEventListener('click', clearLabel);

  // accounts
  document.getElementById('btn-refresh-accounts').addEventListener('click', loadAccounts);
  document.getElementById('btn-new-account').addEventListener('click', () => openAccount(null));
  document.getElementById('btn-acct-cancel').addEventListener('click', () => document.getElementById('dlg-account').close());
  document.getElementById('form-account').addEventListener('submit', saveAccount);
  document.getElementById('btn-acct-test').addEventListener('click', () => {
    const id = document.getElementById('a-id').value;
    testAccount(id ? Number(id) : 0);
  });

  // approvals
  document.getElementById('btn-refresh-approvals').addEventListener('click', loadApprovals);
  document.getElementById('flt-approval-status').addEventListener('change', loadApprovals);
  document.getElementById('btn-refresh-logs').addEventListener('click', loadLogs);

  // filters
  document.getElementById('flt-status').addEventListener('change', loadTodos);
  document.getElementById('flt-cat').addEventListener('change', loadTodos);
  document.getElementById('flt-party').addEventListener('change', loadTodos);
  document.getElementById('flt-q').addEventListener('input', debounce(loadTodos, 300));
  document.getElementById('flt-notice-status').addEventListener('change', loadNotices);
  document.getElementById('flt-notice-cat').addEventListener('change', loadNotices);
  document.getElementById('flt-notice-q').addEventListener('input', debounce(loadNotices, 300));
  document.getElementById('flt-mail-cat').addEventListener('change', loadEmails);
  document.getElementById('flt-mail-label').addEventListener('change', loadEmails);
  document.getElementById('flt-mail-q').addEventListener('input', debounce(loadEmails, 300));
  document.getElementById('flt-mail-since').addEventListener('change', loadEmails);
  document.getElementById('flt-mail-before').addEventListener('change', loadEmails);
  document.getElementById('flt-log-status').addEventListener('change', loadLogs);

  loadConfig().then(() => {
    loadTodos();
    loadNotices();
    loadParties();
    loadEmails();
    loadAccounts();
    loadApprovals();
    loadLogs();
  });
});

function debounce(fn, ms) {
  let t;
  return (...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}
