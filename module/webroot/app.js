/* Local-only KernelSU bridge. No credentials or server data enter HTML markup. */
'use strict';
(() => {
  const NPCTL = '/data/adb/modules/natpierce_rs/bin/npctl';
  const $ = id => document.getElementById(id);
  const state = { daemon: false, core_running: false, logged_in: false, phase: 'unknown', tun: {}, peers: [], server_clients: [] };
  let core = { installed: false, verified: false, downloading: false, phase: 'unknown', version: '', installed_version: '', available_version: '', update_available: false, checking_update: false, update_error: '', bytes_downloaded: 0, error: '' }, coreLoaded = false, coreAction = '';
  let settings = { autostart: true, auto_connect: true, role: 'idle', pcname: '', client: {}, server: {} };
  let page = 'connect', logKind = 'daemon', requestQueue = Promise.resolve(), statusPending = false;
  let actionBusy = false, settingsDirty = false, settingsLoaded = false, pollTimer, toastTimer, counter = 0, transportError = '';
  const activeSecrets = new Set();
  const actionButtons = ['primary-action', 'disconnect', 'server-stop', 'logout', 'service-start', 'service-restart', 'service-stop', 'run-doctor', 'refresh-logs'];

  const THEME_KEY = 'natpierce.webui.theme.v1';
  const CYAN = '#00838f';
  const systemAppearance = window.matchMedia('(prefers-color-scheme: dark)');
  let theme = { source: 'cyan', mode: 'system', color: CYAN };
  try {
    const saved = JSON.parse(localStorage.getItem(THEME_KEY) || '{}');
    if (['cyan', 'system', 'custom'].includes(saved.source)) theme.source = saved.source;
    if (['system', 'dark', 'light'].includes(saved.mode)) theme.mode = saved.mode;
    if (/^#[0-9a-f]{6}$/i.test(saved.color || '')) theme.color = saved.color.toLowerCase();
  } catch (_) {}
  function rgb(value) { return [1, 3, 5].map(index => parseInt(value.slice(index, index + 2), 16) / 255); }
  function hex(values) { return '#' + values.map(value => Math.round(Math.max(0, Math.min(1, value)) * 255).toString(16).padStart(2, '0')).join(''); }
  function mix(a, b, amount) { const x = rgb(a), y = rgb(b); return hex(x.map((value, index) => value * (1 - amount) + y[index] * amount)); }
  function luminance(value) { return rgb(value).map(x => x <= .04045 ? x / 12.92 : ((x + .055) / 1.055) ** 2.4).reduce((sum, x, index) => sum + x * [.2126, .7152, .0722][index], 0); }
  function contrast(a, b) { const x = luminance(a), y = luminance(b); return (Math.max(x, y) + .05) / (Math.min(x, y) + .05); }
  function hueSaturation(color) {
    const [r, g, b] = rgb(color), maximum = Math.max(r, g, b), minimum = Math.min(r, g, b), delta = maximum - minimum;
    const lightness = (maximum + minimum) / 2;
    if (!delta) return [0, 0];
    let hue = maximum === r ? (g - b) / delta + (g < b ? 6 : 0) : maximum === g ? (b - r) / delta + 2 : (r - g) / delta + 4;
    return [hue / 6, delta / (1 - Math.abs(2 * lightness - 1))];
  }
  function tone(h, s, l) {
    if (!s) return hex([l, l, l]);
    const q = l < .5 ? l * (1 + s) : l + s - l * s, p = 2 * l - q;
    const channel = t => { t = (t + 1) % 1; return t < 1 / 6 ? p + (q - p) * 6 * t : t < .5 ? q : t < 2 / 3 ? p + (q - p) * (2 / 3 - t) * 6 : p; };
    return hex([channel(h + 1 / 3), channel(h), channel(h - 1 / 3)]);
  }
  function themeExtras(palette, dark) {
    if (contrast(palette.primary, palette['on-primary']) < 4.5) palette['on-primary'] = contrast(palette.primary, '#000000') > contrast(palette.primary, '#ffffff') ? '#000000' : '#ffffff';
    palette['error-bg'] = mix(palette.surface, palette.error, dark ? .14 : .1);
    palette['warn-bg'] = mix(palette.surface, palette.warn, dark ? .12 : .08);
    palette['chip-bg'] = mix(palette.surface, palette.primary, dark ? .13 : .1);
    palette.divider = mix(palette.surface, palette.line, .38);
    palette['log-bg'] = mix(palette.bg, dark ? '#000000' : '#ffffff', .2);
    palette.inverse = palette.text;
    palette['on-inverse'] = palette.bg;
    palette['on-error'] = luminance(palette.error) > .4 ? '#3b1013' : '#ffffff';
    return palette;
  }
  function paletteFor(seed, dark) {
    const [h, saturation] = hueSaturation(seed), s = saturation < .03 ? 0 : Math.max(.5, Math.min(.82, saturation)), neutral = saturation < .03 ? 0 : .08;
    return {
      bg: tone(h, neutral, dark ? .065 : .985), surface: tone(h, neutral, dark ? .105 : .96),
      'surface-high': tone(h, neutral, dark ? .155 : .92), 'surface-highest': tone(h, neutral, dark ? .205 : .875),
      primary: tone(h, s, dark ? .78 : .29), 'on-primary': tone(h, s, dark ? .115 : .985),
      container: tone(h, s * .65, dark ? .235 : .89), 'on-container': tone(h, s * .55, dark ? .91 : .15),
      text: tone(h, neutral, dark ? .92 : .105), muted: tone(h, neutral, dark ? .77 : .30),
      faint: tone(h, neutral, dark ? .66 : .42), line: tone(h, neutral, dark ? .34 : .63),
      success: dark ? '#a5d6b1' : '#1c6b39', warn: dark ? '#eecb91' : '#775300', error: dark ? '#ffb4ab' : '#b3261e'
    };
  }
  function managerPalette() {
    // BakaSU's real local internal/colors.css exposes these Material tokens.
    // Our np-prefixed colors intentionally never shadow the manager's tokens.
    const style = getComputedStyle(document.documentElement);
    const read = name => { const value = style.getPropertyValue('--' + name).trim(); return /^#[0-9a-f]{6}(?:[0-9a-f]{2})?$/i.test(value) ? value.slice(0, 7).toLowerCase() : ''; };
    const palette = { primary: read('primary'), 'on-primary': read('onPrimary'), surface: read('surface'), text: read('onSurface') };
    if (Object.values(palette).some(value => !value)) return null;
    for (const [key, name] of Object.entries({ bg: 'background', 'surface-high': 'surfaceContainerHigh', 'surface-highest': 'surfaceContainerHighest', container: 'primaryContainer', 'on-container': 'onPrimaryContainer', muted: 'onSurfaceVariant', line: 'outline', error: 'error' })) {
      const value = read(name); if (value) palette[key] = value;
    }
    return palette;
  }
  function applyTheme() {
    const dark = theme.mode === 'dark' || theme.mode === 'system' && systemAppearance.matches;
    const dynamic = theme.source === 'system' ? managerPalette() : null;
    const seed = theme.source === 'custom' ? theme.color : dynamic ? dynamic.primary : CYAN;
    let palette = paletteFor(seed, dark);
    if (dynamic && (luminance(dynamic.surface) < luminance(dynamic.text)) === dark) palette = { ...palette, ...dynamic };
    palette = themeExtras(palette, dark);
    const root = document.documentElement;
    for (const [key, value] of Object.entries(palette)) root.style.setProperty('--np-' + key, value);
    root.style.colorScheme = dark ? 'dark' : 'light';
    root.dataset.theme = dark ? 'dark' : 'light'; root.dataset.themeSource = theme.source;
    document.querySelector('meta[name="theme-color"]').content = palette.bg;
    $('theme-source').value = theme.source; $('theme-mode').value = theme.mode;
    $('theme-color').value = theme.color; $('theme-hex').value = theme.color;
    $('theme-custom').hidden = theme.source !== 'custom';
    $('theme-hint').textContent = theme.source === 'system' ? dynamic ? '使用管理器提供的颜色。设置自动保存。' : '管理器没有提供动态配色，已使用默认青色。' : '修改后立即生效，并保存在本机。';
  }
  function saveTheme(update) {
    theme = { ...theme, ...update };
    try { localStorage.setItem(THEME_KEY, JSON.stringify(theme)); } catch (_) {}
    applyTheme();
  }
  function initTheme() {
    $('theme-source').addEventListener('change', event => saveTheme({ source: event.target.value }));
    $('theme-mode').addEventListener('change', event => saveTheme({ mode: event.target.value }));
    $('theme-color').addEventListener('input', event => saveTheme({ source: 'custom', color: event.target.value }));
    $('theme-hex').addEventListener('input', event => {
      const valid = /^#[0-9a-f]{6}$/i.test(event.target.value);
      event.target.setAttribute('aria-invalid', String(!valid));
      if (valid) saveTheme({ source: 'custom', color: event.target.value.toLowerCase() });
    });
    $('theme-hex').addEventListener('blur', () => { $('theme-hex').value = theme.color; $('theme-hex').removeAttribute('aria-invalid'); });
    document.querySelectorAll('[data-color]').forEach(button => button.addEventListener('click', () => saveTheme({ source: 'custom', color: button.dataset.color })));
    $('theme-reset').addEventListener('click', () => { saveTheme({ source: 'cyan', mode: 'system', color: CYAN }); toast('外观已恢复默认。'); });
    if (systemAppearance.addEventListener) systemAppearance.addEventListener('change', applyTheme); else systemAppearance.addListener(applyTheme);
    $('manager-colors').addEventListener('load', applyTheme); $('manager-colors').addEventListener('error', applyTheme);
    applyTheme();
  }

  function bridge() {
    try { if (window.ksu && typeof window.ksu.exec === 'function') return window.ksu; } catch (_) {}
    try { if (window.top.ksu && typeof window.top.ksu.exec === 'function') return window.top.ksu; } catch (_) {}
    return null;
  }
  function encode(payload) {
    const bytes = new TextEncoder().encode(JSON.stringify(payload));
    let binary = '';
    for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    return btoa(binary);
  }
  function clean(value) {
    let text = String(value == null ? '' : value);
    for (const secret of activeSecrets) if (secret) text = text.split(secret).join('••••••');
    return text.replace(/((?:password|passwd|conpwd|webpwd|token|authorization|密码)\s*["']?\s*[=:]\s*)(?:"[^"]*"|'[^']*'|[^\s,;}\]]+)/gi, '$1[已隐藏]');
  }
  function exec(command) {
    const api = bridge();
    if (!api) return Promise.reject(new Error('请在 KernelSU 管理器中打开此页面。'));
    return new Promise((resolve, reject) => {
      let finished = false;
      const callback = '__nprs_' + Date.now() + '_' + (++counter);
      const finish = (error, result) => {
        if (finished) return;
        finished = true;
        clearTimeout(timer);
        delete window[callback];
        error ? reject(error) : resolve(result);
      };
      const timer = setTimeout(() => finish(new Error('服务响应超时，操作可能仍在进行。请刷新状态。')), 3800);
      window[callback] = (errno, stdout, stderr) => finish(null, { errno: Number(errno) || 0, stdout: String(stdout || ''), stderr: String(stderr || '') });
      try {
        const returned = api.exec(command, '{}', callback);
        const normalize = result => {
          if (result && typeof result === 'object') return { errno: Number(result.errno || result.code) || 0, stdout: String(result.stdout || ''), stderr: String(result.stderr || '') };
          return { errno: 0, stdout: String(result == null ? '' : result), stderr: '' };
        };
        if (returned && typeof returned.then === 'function') returned.then(value => finish(null, normalize(value)), error => finish(error));
        else if (returned && (typeof returned === 'object' || typeof returned === 'string')) finish(null, normalize(returned));
      } catch (error) { finish(error); }
    });
  }
  function request(action, args = {}) {
    const payload = encode({ ...args, action });
    const operation = requestQueue.then(async () => {
      const result = await exec(NPCTL + ' request --b64 ' + payload);
      let data;
      try { data = JSON.parse(result.stdout.trim()); } catch (_) { throw new Error(clean(result.stderr || '无法读取服务状态。')); }
      if (!data || typeof data !== 'object') throw new Error('服务返回的数据格式有误。');
      if (data.core && typeof data.core === 'object') { core = { ...core, ...data.core }; coreLoaded = true; }
      if (data.state && typeof data.state === 'object') Object.assign(state, data.state);
      if (data.settings && typeof data.settings === 'object') {
        settings = { ...settings, ...data.settings, client: { ...settings.client, ...data.settings.client }, server: { ...settings.server, ...data.settings.server } };
        populateSettings();
      }
      render();
      if (data.ok === false || result.errno !== 0) throw new Error(clean(data.error || '操作失败，请查看诊断。'));
      return data;
    });
    requestQueue = operation.catch(() => {});
    return operation;
  }
  function toast(message, error = false) {
    $('snackbar').textContent = clean(message);
    $('snackbar').classList.toggle('error', error);
    $('snackbar').hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { $('snackbar').hidden = true; }, error ? 6500 : 3500);
  }
  function text(id, value, fallback = '—') { $(id).textContent = clean(value == null || value === '' ? fallback : value); }
  function isClientVerified() { return state.phase === 'client' && !!state.client_connected && !!state.data_ok; }
  function isConnected() { return isClientVerified() || isServer(); }
  function isServer() { return state.phase === 'server' && !!state.core_running; }
  function coreMissing() { return state.phase === 'core_missing' || coreLoaded && (!core.installed || !core.verified); }
  function coreBusy() { return !!(core.downloading || core.checking_update || coreAction); }
  function isPending() { return ['starting', 'logging-in', 'logging_in', 'connecting', 'reconnecting', 'restoring', 'waiting-network', 'waiting_network'].includes(state.phase); }
  function bytes(value) {
    const n = Number(value);
    if (!Number.isFinite(n) || n < 0) return '0 B';
    if (n < 1024) return Math.floor(n) + ' B';
    if (n < 1048576) return (n / 1024).toFixed(1) + ' KB';
    if (n < 1073741824) return (n / 1048576).toFixed(1) + ' MB';
    return (n / 1073741824).toFixed(1) + ' GB';
  }
  function uptime(value) {
    const n = Math.floor(Number(value));
    if (!Number.isFinite(n) || n < 0) return '—';
    if (n < 60) return n + ' 秒';
    if (n < 3600) return Math.floor(n / 60) + ' 分钟';
    if (n < 86400) return Math.floor(n / 3600) + ' 小时 ' + Math.floor((n % 3600) / 60) + ' 分钟';
    return Math.floor(n / 86400) + ' 天 ' + Math.floor((n % 86400) / 3600) + ' 小时';
  }
  function controls() {
    const unavailable = !bridge() || actionBusy;
    document.querySelectorAll('form button[type="submit"], [data-login], .device button').forEach(button => { button.disabled = unavailable; });
    actionButtons.forEach(id => { $(id).disabled = unavailable; });
    $('logout').disabled = unavailable || !state.logged_in;
    $('disconnect').disabled = unavailable || (!isConnected() && !isPending() && state.phase !== 'client');
    $('server-stop').disabled = unavailable || !isServer();
    $('service-start').disabled = unavailable || !!state.daemon;
    $('service-stop').disabled = unavailable || !state.daemon;
    $('service-restart').disabled = unavailable || !state.daemon;
    document.querySelectorAll('[data-core-download]').forEach(button => { button.disabled = unavailable || coreBusy(); });
    $('core-check-update').disabled = unavailable || coreBusy();
    $('core-update').disabled = unavailable || coreBusy() || !core.update_available || coreMissing();
    if (coreMissing()) {
      document.querySelectorAll('#connect-form button[type="submit"], #server-form button[type="submit"], [data-login], .device button').forEach(button => { button.disabled = true; });
      $('primary-action').disabled = unavailable || coreBusy();
    }
  }
  function renderCore() {
    const missing = coreMissing();
    const phase = core.downloading ? core.phase === 'verifying' ? '校验中' : core.phase === 'installing' ? '安装中' : '下载中' : core.phase === 'error' ? '操作失败' : core.installed && core.verified ? '已安装' : coreLoaded ? '尚未安装' : '读取中';
    document.querySelectorAll('[data-core-setup]').forEach(card => { card.hidden = !missing; });
    document.querySelectorAll('[data-core-phase]').forEach(element => { element.textContent = phase; });
    document.querySelectorAll('[data-core-progress]').forEach(element => { element.hidden = !core.downloading; element.removeAttribute('value'); });
    const downloaded = Number(core.bytes_downloaded) > 0 ? ' · 已下载 ' + bytes(core.bytes_downloaded) : '';
    const message = core.downloading ? (core.phase === 'verifying' ? '正在校验文件' : core.phase === 'installing' ? '正在安装核心' : '正在从皎月连官网下载') + downloaded : core.error ? '下载未完成，请查看下方错误后重试。' : missing ? '下载完成后即可登录使用。' : '文件校验通过。';
    document.querySelectorAll('[data-core-state]').forEach(element => { element.textContent = message; });
    document.querySelectorAll('[data-core-error]').forEach(element => { element.hidden = !core.error; element.textContent = clean(core.error); });
    document.querySelectorAll('[data-core-download]').forEach(button => { button.textContent = core.downloading ? '正在下载…' : core.phase === 'error' ? '重新下载官方核心' : '下载官方核心'; });
    const installedVersion = core.installed_version || (core.installed ? core.version : '');
    const availableVersion = core.available_version || core.version;
    document.querySelectorAll('[data-core-version]').forEach(element => { element.textContent = clean(availableVersion || installedVersion || '读取中'); });
    text('core-info-status', phase);
    text('core-info-version', installedVersion, coreLoaded ? '尚未安装' : '读取中');
    text('core-info-available-version', availableVersion, '尚未读取');
    text('core-info-verified', !coreLoaded ? '读取中' : core.verified ? '校验通过' : core.installed ? '校验未通过' : '尚未下载');
    text('core-info-sha', core.sha256);
    const checking = core.checking_update || coreAction === 'core-check-update';
    const updateError = core.update_error || core.error;
    const updateMessage = checking ? '正在检查更新…' : core.downloading ? message : coreAction === 'core-update' ? '正在更新…' : coreAction === 'core-download' ? '正在下载…' : updateError ? '检查或更新失败，请查看下方错误。' : core.update_available ? '有新版本，可以更新。' : installedVersion && availableVersion ? '已是推荐版本' : '点击「检查更新」查看是否有新版。';
    text('core-update-state', updateMessage);
    $('core-update-error').hidden = !updateError; text('core-update-error', updateError);
    $('core-update-progress').hidden = !core.downloading; $('core-update-progress').removeAttribute('value');
    $('core-check-update').textContent = checking ? '正在检查…' : '检查更新';
    $('core-update').hidden = !core.update_available && coreAction !== 'core-update';
    $('core-update').textContent = core.downloading || coreAction === 'core-update' ? '正在更新…' : '更新核心';
  }
  function render() {
    const connected = isConnected(), pending = isPending(), server = isServer();
    let title = '连接设备', label = '已登录', detail = '选择在线设备，或输入对方的识别码。';
    if (transportError) { title = '无法读取状态'; label = '状态未知'; detail = transportError; }
    else if (coreMissing()) { title = core.downloading ? '正在下载核心' : '请先下载核心'; label = core.downloading ? '下载中' : '尚未安装'; detail = core.downloading ? '关闭页面不会中断下载，完成后自动使用已保存的设置。' : '核心从皎月连官网下载，完成后即可登录使用。'; }
    else if (!state.daemon && state.phase !== 'unknown') { title = '后台服务已停止'; label = '已停止'; detail = '启动服务后可继续使用。'; }
    else if (state.phase === 'unknown') { title = '正在读取状态'; label = '读取中'; detail = '正在读取后台服务状态。'; }
    else if (pending) {
      title = ['logging-in', 'logging_in'].includes(state.phase) ? '正在登录' : ['waiting-network', 'waiting_network'].includes(state.phase) ? '等待网络恢复' : '正在连接';
      label = '处理中'; detail = '关闭页面不会中断操作。';
    } else if (server) { title = '服务端已启动'; label = '运行中'; detail = '本机识别码：' + String(state.code || '正在获取'); }
    else if (connected) { title = '连接成功'; label = '已连接'; detail = state.tun && state.tun.ip ? '可以通过虚拟 IP 访问对方设备。' : '正在读取虚拟网卡信息。'; }
    else if (state.error || state.phase === 'error') { title = '连接出错'; label = '错误'; detail = clean(state.error || '请打开诊断查看原因。'); }
    else if (state.phase === 'client' && state.client_connected) { title = '正在检查连接'; label = '会话已建立'; detail = '连接已建立，正在检查数据是否能传输。'; }
    else if (state.phase === 'client') { title = '正在连接'; label = '等待对方响应'; detail = '连接仍在进行，状态会自动更新。'; }
    else if (!state.logged_in) { title = '登录皎月连'; label = '未登录'; detail = '登录后可连接其他设备，或启动服务端。'; }
    text('status-title', title); text('status-label', label); text('status-detail', detail);
    text('role-chip', server ? '服务端' : settings.role === 'client' ? '客户端' : '待机');
    const downloading = coreMissing() && !!core.downloading;
    const visualError = !!transportError || !downloading && !pending && (coreMissing() ? !!core.error : !!state.error);
    $('hero').classList.toggle('connected', connected && !transportError && !coreMissing());
    $('hero').classList.toggle('error', visualError);
    $('status-dot').className = 'dot' + (visualError ? ' bad' : pending || downloading ? ' pending' : connected && !coreMissing() ? ' good' : '');
    text('virtual-ip', state.tun && state.tun.ip);
    text('traffic', state.tun && state.tun.present ? bytes(state.tun.rx_bytes) + ' / ' + bytes(state.tun.tx_bytes) : '—');
    $('account-nudge').hidden = !!state.logged_in;
    $('disconnect').hidden = !connected && !pending && state.phase !== 'client';
    text('primary-action', coreMissing() ? core.downloading ? '正在下载…' : '下载官方核心' : !state.daemon && state.phase !== 'unknown' ? '启动服务' : !state.logged_in ? '登录账号' : server ? '查看服务端' : '输入识别码');
    text('account-name', state.user || (state.logged_in ? '账号已登录' : '未登录皎月连'));
    text('account-status', state.logged_in ? '已登录' : '未登录');
    text('daemon-state', state.daemon ? '运行中' : '已停止');
    text('core-state', state.core_running ? '运行中' : '未运行');
    text('uptime', uptime(state.uptime)); text('local-ip', state.local_ip);
    text('server-code', state.code, '— — — —'); text('server-label', server ? '服务端运行中' : '服务端未启动');
    $('server-dot').className = 'dot' + (server ? ' good' : '');
    const clients = Array.isArray(state.server_clients) ? state.server_clients : [];
    text('client-count', clients.length + ' 台连接');
    devices('server-clients', clients, false);
    const peers = Array.isArray(state.peers) ? state.peers : [];
    text('peer-count', peers.length + ' 台');
    devices('peers', peers, true);
    text('last-event', state.last_event, '暂无记录');
    $('last-error').hidden = !state.error;
    text('last-error', state.error, '');
    renderCore();
    controls();
  }
  function devices(id, rows, canConnect) {
    const list = $(id);
    const fragment = document.createDocumentFragment();
    if (!rows.length) {
      const empty = document.createElement('p'); empty.className = 'empty';
      empty.textContent = canConnect ? (state.logged_in ? '暂无在线设备' : '登录后查看设备') : '暂无设备连接';
      fragment.appendChild(empty);
    }
    for (const peer of rows.slice(0, 200)) {
      if (!peer || typeof peer !== 'object') continue;
      const row = document.createElement('div'); row.className = 'device';
      const avatar = document.createElement('span'); avatar.className = 'device-avatar'; avatar.textContent = canConnect ? '端' : '客';
      const info = document.createElement('div'); info.className = 'device-info';
      const name = document.createElement('div'); name.className = 'device-name'; name.textContent = clean(peer.name || peer.pcname || '未命名设备');
      const detail = document.createElement('div'); detail.className = 'device-id'; detail.textContent = clean(peer.ip || peer.id || peer.code || '');
      info.append(name, detail); row.append(avatar, info);
      if (canConnect) {
        const button = document.createElement('button'); button.className = 'button tonal'; button.textContent = '连接'; button.type = 'button';
        button.disabled = !bridge() || actionBusy || peer.id == null;
        button.addEventListener('click', () => runAction('connect-peer', { id: String(peer.id) }, '正在连接…'));
        row.append(button);
      }
      fragment.appendChild(row);
    }
    list.replaceChildren(fragment);
  }
  function populateSettings() {
    if (settingsDirty && settingsLoaded) return;
    $('autostart').checked = settings.autostart !== false;
    $('auto-connect').checked = settings.auto_connect !== false;
    $('device-name').value = settings.pcname || '';
    $('startup-role').value = ['client', 'server'].includes(settings.role) ? settings.role : 'idle';
    if (!settingsLoaded) {
      $('client-code').value = settings.client.code || '';
      $('server-lan').value = settings.server.lanip || '';
    }
    settingsLoaded = true;
  }
  function switchPage(view) {
    page = view;
    document.querySelectorAll('.view').forEach(section => { section.hidden = section.id !== 'view-' + view; });
    document.querySelectorAll('[data-view]').forEach(button => {
      const active = button.dataset.view === view;
      button.classList.toggle('active', active);
      if (active) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current');
    });
    window.scrollTo({ top: 0, behavior: 'auto' });
  }
  async function poll(manual = false) {
    if (statusPending || actionBusy || !bridge() || document.hidden && !manual) return;
    statusPending = true;
    if (manual) $('refresh').classList.add('spinning');
    try {
      transportError = '';
      await request('status');
    } catch (error) { transportError = clean(error.message); render(); if (manual) toast(error.message, true); }
    finally { statusPending = false; $('refresh').classList.remove('spinning'); }
  }
  function schedulePoll() {
    clearTimeout(pollTimer);
    pollTimer = setTimeout(async () => { await poll(); schedulePoll(); }, isPending() || coreBusy() ? 1500 : 3500);
  }
  async function runAction(action, args = {}, message = '正在处理…') {
    if (actionBusy || !bridge()) return null;
    actionBusy = true; controls();
    for (const key of ['password', 'conpwd', 'webpwd']) if (args[key]) activeSecrets.add(String(args[key]));
    try {
      transportError = '';
      const data = await request(action, args);
      if (message) toast(message);
      return data;
    } catch (error) { toast(error.message, true); if (action === 'login') { text('login-error', error.message); $('login-error').hidden = false; } return null; }
    finally { actionBusy = false; controls(); await poll(); schedulePoll(); }
  }
  async function runCoreAction(action) {
    if (coreBusy() || actionBusy || !bridge()) return;
    coreAction = action; render();
    try { await runAction(action, {}, ''); }
    finally { coreAction = ''; render(); schedulePoll(); }
  }
  function openLogin() {
    $('login-user').value = state.user || '';
    $('login-password').value = '';
    $('login-error').hidden = true;
    $('login-dialog').showModal();
    $('login-user').focus();
  }
  function closeLogin() { $('login-password').value = ''; $('login-dialog').close(); }
  document.querySelectorAll('[data-view]').forEach(button => button.addEventListener('click', () => switchPage(button.dataset.view)));
  document.querySelectorAll('[data-login]').forEach(button => button.addEventListener('click', openLogin));
  $('close-login').addEventListener('click', closeLogin);
  $('login-dialog').addEventListener('close', () => { $('login-password').value = ''; });
  $('login-form').addEventListener('submit', async event => {
    event.preventDefault();
    const user = $('login-user').value.trim(), password = $('login-password').value;
    if (!user || !password) return;
    $('login-password').value = '';
    $('login-error').hidden = true;
    const data = await runAction('login', { user, password }, '正在登录…');
    if (data) closeLogin();
  });
  $('primary-action').addEventListener('click', () => {
    if (coreMissing()) runCoreAction('core-download');
    else if (!state.daemon && state.phase !== 'unknown') runAction('start', {}, '正在启动后台服务…');
    else if (!state.logged_in) openLogin();
    else if (isServer()) switchPage('server');
    else { $('code-card').scrollIntoView({ behavior: 'smooth', block: 'center' }); $('client-code').focus({ preventScroll: true }); }
  });
  $('connect-form').addEventListener('submit', event => {
    event.preventDefault();
    const code = $('client-code').value.trim(), password = $('client-password').value;
    if (!/^[0-9]{8}$/.test(code)) { toast('请输入 8 位数字识别码。', true); return; }
    $('client-password').value = '';
    runAction('connect', { code, password }, '正在连接…');
  });
  $('server-form').addEventListener('submit', event => {
    event.preventDefault();
    const conpwd = $('server-password').value, webpwd = $('server-web-password').value, lanip = $('server-lan').value.trim();
    if (webpwd && (webpwd.length < 6 || webpwd.length > 20)) { toast('Web 访问密码需要 6–20 位。', true); return; }
    if (lanip && (!/^(?:[0-9]{1,3}\.){3}[0-9]{1,3}$/.test(lanip) || lanip.split('.').some(octet => Number(octet) > 255))) { toast('局域网地址格式有误，例如 192.168.1.10。', true); return; }
    $('server-password').value = ''; $('server-web-password').value = '';
    runAction('server-start', { conpwd, webpwd, lanip }, '正在启动服务端…');
  });
  $('settings-form').addEventListener('input', () => { settingsDirty = true; });
  $('settings-form').addEventListener('change', () => { settingsDirty = true; });
  $('settings-form').addEventListener('submit', async event => {
    event.preventDefault();
    const updated = { ...settings, autostart: $('autostart').checked, auto_connect: $('auto-connect').checked, pcname: $('device-name').value.trim(), role: $('startup-role').value };
    const data = await runAction('settings-save', { settings: updated }, '设置已保存。');
    if (data) { settings = { ...settings, ...updated }; settingsDirty = false; populateSettings(); }
  });
  $('disconnect').addEventListener('click', () => runAction('disconnect', {}, '正在断开连接…'));
  $('server-stop').addEventListener('click', () => runAction('server-stop', {}, '正在停止服务端…'));
  $('logout').addEventListener('click', () => runAction('logout', {}, '已退出登录。'));
  $('service-start').addEventListener('click', () => runAction('start', {}, '正在启动后台服务…'));
  $('service-restart').addEventListener('click', () => runAction('restart', {}, '正在重启后台服务…'));
  $('service-stop').addEventListener('click', () => runAction('stop', {}, '后台服务已停止。'));
  $('refresh').addEventListener('click', () => poll(true));
  document.querySelectorAll('[data-core-download]').forEach(button => button.addEventListener('click', () => runCoreAction('core-download')));
  $('core-check-update').addEventListener('click', () => runCoreAction('core-check-update'));
  $('core-update').addEventListener('click', () => runCoreAction('core-update'));
  $('run-doctor').addEventListener('click', async () => {
    const data = await runAction('doctor', {}, '');
    if (!data) return;
    const result = data.diagnostics || data.checks || data.doctor || data.data || data.state || {};
    const labels = { tun_device: '虚拟网卡', control_connected: '控制通道', core_running: '核心进程', logged_in: '账号登录', route: '路由', data_verified: '数据连通', probe_ip: '探测地址', probe_port: '探测端口', dns_loopback: 'DNS 解析' };
    const fragment = document.createDocumentFragment();
    const entries = Array.isArray(result) ? result.map(item => [item.name || item.label || '检查项', item.ok == null ? item.value || item.message : item.ok ? '正常' : item.message || '未通过']) : Object.entries(result);
    for (const [key, value] of entries) {
      if (/(password|conpwd|webpwd|token|user)/i.test(key)) continue;
      const row = document.createElement('div'); row.className = 'diagnostic-line';
      const name = document.createElement('span'); name.textContent = clean(labels[key] || key);
      const status = document.createElement('span'); status.textContent = clean(typeof value === 'object' ? JSON.stringify(value) : value === true ? '正常' : value === false ? '未通过' : value);
      if (typeof value === 'boolean') status.className = value ? 'good' : 'bad';
      row.append(name, status); fragment.append(row);
    }
    if (!entries.length) { const empty = document.createElement('p'); empty.className = 'diagnostic-empty'; empty.textContent = '暂无诊断结果。'; fragment.append(empty); }
    $('doctor-result').className = ''; $('doctor-result').replaceChildren(fragment);
  });
  async function loadLogs() {
    const data = await runAction('logs', { kind: logKind }, '');
    if (!data) return;
    const value = data.text || data.logs || data.log || data.data || '暂无运行日志。';
    text('log-output', Array.isArray(value) ? value.join('\n') : value);
  }
  $('refresh-logs').addEventListener('click', loadLogs);
  document.querySelectorAll('[data-log]').forEach(button => button.addEventListener('click', () => {
    if (actionBusy) return;
    logKind = button.dataset.log;
    document.querySelectorAll('[data-log]').forEach(item => { item.classList.toggle('selected', item === button); item.setAttribute('aria-pressed', String(item === button)); });
    loadLogs();
  }));
  document.addEventListener('visibilitychange', () => { if (!document.hidden) { applyTheme(); poll(); schedulePoll(); } });
  window.addEventListener('pagehide', () => { clearTimeout(pollTimer); activeSecrets.clear(); });
  $('bridge-notice').hidden = !!bridge();
  initTheme();
  render();
  if (bridge()) { poll().finally(schedulePoll); } else { transportError = '请在 KernelSU 管理器的模块页面打开 WebUI。'; render(); }
})();
