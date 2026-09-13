'use strict';
const { invoke, convertFileSrc } = window.__TAURI__.core;
const el = id => document.getElementById(id);
const setText = (id, text) => {
  const node = el(id);
  if (!node) return;
  const label = node.querySelector('.label');
  if (label) label.textContent = text;
  else node.textContent = text;
};
let state, config, rows = [], selected, busy = false, lastArchive = '', wasRendering = false, monitorEnabled = false;
let pendingGain, gainTask, gainTimer, recordingPending = false, renaming;
let preferredOriginal = false, playerKey = '', playerGeneration = 0, playerLoad = Promise.resolve();
// A separate small IPC snapshot keeps the meters responsive without refreshing
// diagnostics or the recording library at audio-meter cadence.
const levelViews = ['input', 'left', 'right'].map(name => {
  const root = el(`level-${name}`);
  return { root, track: root.querySelector('.level-track'), fill: root.querySelector('.level-fill'),
    hold: root.querySelector('.level-hold'), number: root.querySelector('.level-number'),
    clip: root.querySelector('.level-clip'), db: -60, peak: -60, holdUntil: 0, clipUntil: 0 };
});
let levelRequest = false, levelReceived = 0, levelTick = performance.now(), levelWasHidden = false;
function drawLevels(peaks, active) {
  const now = performance.now(), dt = Math.min((now - levelTick) / 1000, 1);
  levelTick = now;
  levelViews.forEach((v, i) => {
    const amplitude = active && Number.isFinite(peaks[i]) ? Math.max(0, peaks[i]) : 0;
    const db = amplitude > 0 ? 20 * Math.log10(amplitude) : -Infinity;
    v.db = active ? Math.max(-60, db, v.db - 30 * dt) : -60;
    if (!active) { v.peak = -60; v.holdUntil = 0; v.clipUntil = 0; }
    else if (db >= v.peak) { v.peak = Math.max(-60, db); v.holdUntil = now + 1000; }
    else if (now > v.holdUntil) v.peak = Math.max(v.db, v.peak - 30 * dt);
    if (amplitude >= 1) v.clipUntil = now + 2000;
    const clipped = now < v.clipUntil;
    v.fill.style.transform = `scaleX(${Math.min(1, (v.db + 60) / 60)})`;
    v.hold.style.left = `${Math.min(100, Math.max(0, (v.peak + 60) / 60 * 100))}%`;
    v.hold.hidden = v.peak <= -60;
    v.root.classList.toggle('near-clip', v.db >= -6);
    v.root.classList.toggle('clipped', clipped);
    v.clip.hidden = !clipped;
    // The numeric readout holds the sample peak for one second, like the marker.
    const text = v.peak <= -60 ? (active ? '≤−60' : '−∞') : v.peak.toFixed(1);
    v.number.firstChild.textContent = `${text} `;
    v.track.setAttribute('aria-valuenow', Math.min(0, v.db).toFixed(1));
    v.track.setAttribute('aria-valuetext', clipped ? `${text} dBFS、クリップ検出` : `${text} dBFS`);
  });
}
async function pollLevels() {
  if (document.hidden) { levelWasHidden = true; drawLevels([], false); return; }
  if (levelReceived && performance.now() - levelReceived > 500) drawLevels([], false);
  if (levelRequest) return;
  levelRequest = true;
  try {
    const levels = await invoke('audio_levels');
    levelReceived = performance.now();
    // Discard peaks accumulated while minimized instead of replaying old clips.
    drawLevels(levelWasHidden ? [] : levels.peaks, levels.active && !levelWasHidden);
    levelWasHidden = false;
  } catch { drawLevels([], false); }
  finally { levelRequest = false; }
}
setInterval(pollLevels, 50);
function notice(error) {
  el('notice').textContent = String(error);
  el('notice').hidden = !error;
  if (error && (!state || !window.__TAURI__)) {
    el('runtime').hidden = false;
    el('runtime').className = 'status-pill error';
    setText('runtime', 'バックエンド未接続');
  }
}
function action(id, fn) { el(id).addEventListener('click', async () => { if (busy) return; busy = true; notice(''); try { await fn(); await poll(); } catch (e) { notice(e); } finally { busy = false; } }); }
function updateSaveSummary() {
  const root = el('output-root').textContent;
  const name = root.split(/[\\/]/).filter(Boolean).pop() || '未設定';
  const raw = el('save-raw').checked ? '生データあり' : '生データなし';
  el('save-summary').textContent = `${name} · ${raw}`;
  el('save-summary').title = `${root} · ${raw}`;
}
function applyConfig(c) { config = c; el('microphone').value = c.endpoint_id; el('channel').value = c.channel; el('gain').value = c.gain_db; el('gain-value').value = `${c.gain_db} dB`; el('save-raw').checked = c.save_raw; el('output-root').textContent = c.output_root; el('output-root').title = c.output_root; el('monitor-device').value = c.monitor_endpoint; }
function flushGain() {
  clearTimeout(gainTimer);
  gainTimer = undefined;
  if (gainTask) return gainTask;
  gainTask = (async () => {
    while (pendingGain !== undefined) {
      const db = pendingGain;
      pendingGain = undefined;
      const applied = await invoke('configure_gain', { gainDb: db });
      config.gain_db = applied;
    }
  })().catch(async error => {
    pendingGain = undefined;
    // Reconcile the control with the engine after a rejected or failed request.
    const latest = await invoke('studio_status');
    config.gain_db = latest.config.gain_db;
    el('gain').value = config.gain_db;
    el('gain-value').value = `${config.gain_db} dB`;
    throw error;
  }).finally(() => { gainTask = undefined; });
  return gainTask;
}
async function save() { applyConfig(await invoke('configure', { value: { ...config, endpoint_id: el('microphone').value, channel: Number(el('channel').value), gain_db: Number(el('gain').value), save_raw: el('save-raw').checked, monitor_endpoint: el('monitor-device').value } })); }
function select(row) { if (renaming && recordingKey(renaming) !== recordingKey(row.directory)) closeRename(); selected = row; el('selected-name').textContent = row.name || '録音'; el('selected-info').textContent = row.message || ''; el('play-processed').disabled = !row.audio_path; el('play-original').disabled = !row.raw_saved; el('reveal').disabled = false; el('render').disabled = !row.can_reprocess || !!state?.render.active; el('render-gain').value = row.config?.gain_db ?? -18; el('rename').disabled = !canRename(); renderList(); syncPlayer().catch(notice); }
function renderList() {
  const box = el('recordings');
  box.replaceChildren();
  if (!rows.length) {
    const p = document.createElement('p');
    p.className = 'empty';
    p.textContent = '保存した録音がここに並びます。';
    box.append(p);
    return;
  }
  for (const row of rows) {
    const button = document.createElement('button');
    button.className = 'take';
    button.setAttribute('role', 'option');
    button.setAttribute('aria-selected', String(row.directory === selected?.directory));
    const rowTop = document.createElement('div');
      rowTop.className = 'take-heading';
      const title = document.createElement('strong');
      title.textContent = row.name;
      title.title = row.name;
    const badge = document.createElement('span');
    badge.className = 'version-chip';
    badge.textContent = row.can_reprocess ? 'RAW+VSM' : 'VSM';
    rowTop.append(title, badge);
    const info = document.createElement('small');
    info.textContent = row.can_reprocess ? '原音データあり · 再処理可能' : (row.message || 'バイノーラル処理済み');
    button.append(rowTop, info);
    button.addEventListener('click', () => select(row));
    box.append(button);
  }
}
// The folder picker and a just-created take can use ordinary Windows paths,
// while canonical paths returned after finalization include the verbatim prefix.
function recordingKey(path) { return (path || '').replace(/^\\\\\?\\/, '').replaceAll('\\', '/'); }
let refreshGeneration = 0;
async function refresh() {
  const generation = ++refreshGeneration;
  const updated = await invoke('recordings');
  if (generation !== refreshGeneration) return;
  const path = recordingKey(selected?.directory);
  const unique = new Map(updated.map(row => [recordingKey(row.directory), row]));
  if (selected && !unique.has(path)) unique.set(path, selected);
  rows = [...unique.values()];
  const row = unique.get(path) || rows[0];
  if (row) select(row); else { clearPlayer(); renderList(); }
}
function clearPlayer() {
  ++playerGeneration;
  playerKey = '';
  playerLoad = Promise.resolve();
  el('player').pause();
  el('player').removeAttribute('src');
  el('player').load();
  el('playing').textContent = '';
  for (const id of ['play-processed', 'play-original']) {
    el(id).setAttribute('aria-pressed', 'false');
    el(id).classList.remove('btn-primary');
    el(id).classList.add('btn-secondary');
  }
}
async function syncPlayer(autoplay = false) {
  const row = selected;
  if (!row?.audio_path && !row?.raw_saved) {
    clearPlayer();
    el('playing').textContent = row ? '再生できる音声がありません' : '';
    return;
  }
  const original = row.raw_saved && (preferredOriginal || !row.audio_path);
  const key = JSON.stringify([recordingKey(row.directory), !!original, original ? '' : row.audio_path]);
  // Refreshing the list must not reset playback or its seek position.
  if (key !== playerKey) {
    clearPlayer();
    playerKey = key;
    const generation = playerGeneration;
    playerLoad = (async () => {
      try {
        const path = await invoke('recording_audio', { path: row.directory, original: !!original });
        // A slower previous selection must never replace the current recording.
        if (generation !== playerGeneration) return;
        el('player').src = convertFileSrc(path);
        el('player').load();
        el('playing').textContent = original ? '原音 (RAW)' : '処理済み音声';
        const id = original ? 'play-original' : 'play-processed';
        el(id).setAttribute('aria-pressed', 'true');
        el(id).classList.add('btn-primary');
        el(id).classList.remove('btn-secondary');
      } catch (error) {
        if (generation !== playerGeneration) return;
        clearPlayer();
        notice(error);
      }
    })();
  }
  const generation = playerGeneration;
  await playerLoad;
  if (autoplay && generation === playerGeneration && playerKey === key && el('player').getAttribute('src')) {
    try { await el('player').play(); }
    catch (error) { if (generation === playerGeneration) throw error; }
  }
}
async function play(original) { preferredOriginal = original; await syncPlayer(true); }
async function poll() {
  state = await invoke('studio_status');
  const e = state.engine, active = !!e.active, recording = !!e.recording;
  if (recording || !active || e.archive_state === 'failed') recordingPending = false;
  monitorEnabled = state.monitor_enabled;
  if (state.fixture) {
    el('runtime').hidden = false;
    el('runtime').className = 'status-pill warning';
    setText('runtime', 'テストモード（合成音声）');
  } else {
    el('runtime').hidden = true;
  }
  const liveLabel = !active ? '待機中' : (e.pose_ready ? '準備完了' : '位置データ待ち');
  setText('live-state', liveLabel);
  el('live-state').classList.toggle('running', active && e.pose_ready);
  el('live-state').classList.toggle('waiting', active && !e.pose_ready);
  setText('input-toggle', active ? '入力を無効にする' : '入力を有効にする');
  el('input-toggle').classList.toggle('active', active);
  el('record-toggle').disabled = !active;
  setText('record-toggle', recording ? '録音を停止' : '録音を開始');
  el('record-toggle').classList.toggle('recording', recording);
  el('record-state').textContent = recording ? '録音中' : active ? '待機中 · 直前の音声も保持しています' : '';
  el('pose-state').textContent = e.message || '';
  for (const id of ['microphone','channel','save-raw','choose-root']) el(id).disabled = active;
  el('gain').disabled = recording || !!e.gain_locked || recordingPending;
  el('monitor-toggle').disabled = !active;
  setText('monitor-toggle', monitorEnabled ? 'モニター停止' : 'モニター開始');
  el('monitor-state').textContent = state.monitor.error || (state.monitor.ready ? '出力中' : monitorEnabled ? '準備中' : '');
  updateSaveSummary();
  el('connection').textContent = e.network ? JSON.stringify(e.network, null, 2) : '未接続';
  const r = state.render;
  el('cancel').hidden = !r.active;
  el('progress').hidden = !r.active;
  el('progress').value = Number(r.progress) || 0;
  el('render-state').textContent = r.cancelled ? 'キャンセルしました。前回の音声は残っています。' : r.error || r.message || '';
  el('render').disabled = !selected?.can_reprocess || !!r.active;
  el('rename').disabled = !canRename();
  if (e.archive_error) notice(`保存に失敗しました：${e.archive_error}`);
  else if (e.state === 'failed') notice(e.message);
  const archive = e.last_directory || '';
  if ((wasRendering && !r.active) || archive !== lastArchive) {
    lastArchive = archive;
    await refresh();
  }
  wasRendering = !!r.active;
}
function canRename() {
  return !!selected && ['stopped', 'complete', 'failed', 'cancelled'].includes(selected.status) && !state?.render.active;
}
function closeRename() {
  renaming = undefined;
  el('rename-form').hidden = true;
  el('name-display').hidden = false;
  el('rename-error').hidden = true;
}
el('rename').addEventListener('click', () => {
  if (busy || !canRename()) return;
  renaming = selected.directory;
  el('rename-input').value = selected.name;
  el('rename-error').hidden = true;
  el('name-display').hidden = true;
  el('rename-form').hidden = false;
  el('rename-input').focus();
  el('rename-input').select();
});
el('rename-cancel').addEventListener('click', () => { if (!busy) { closeRename(); el('rename').focus(); } });
el('rename-input').addEventListener('keydown', event => {
  if (event.key === 'Escape' && !busy) { event.preventDefault(); closeRename(); el('rename').focus(); }
});
el('rename-form').addEventListener('submit', async event => {
  event.preventDefault();
  if (busy || !renaming) return;
  busy = true;
  el('rename-save').disabled = true;
  el('rename-cancel').disabled = true;
  el('rename-error').hidden = true;
  try {
    const oldPath = renaming;
    // Release the WebView's WAV handle before moving the directory on Windows.
    clearPlayer();
    const row = await invoke('rename_recording', { path: oldPath, name: el('rename-input').value });
    ++refreshGeneration;
    rows = rows.filter(v => recordingKey(v.directory) !== recordingKey(oldPath));
    closeRename();
    select(row);
    await refresh();
    el('rename').focus();
  } catch (error) {
    el('rename-error').textContent = String(error);
    el('rename-error').hidden = false;
    el('rename-input').focus();
  } finally {
    busy = false;
    el('rename-save').disabled = false;
    el('rename-cancel').disabled = false;
  }
});
action('input-toggle', async () => { await flushGain(); if (!state.engine.active) await save(); await invoke('input_enable', { enabled: !state.engine.active }); });
action('record-toggle', async () => {
  const stopping = !!state.engine.recording;
  recordingPending = !stopping;
  el('gain').disabled = true;
  try {
    await flushGain();
    await invoke('record_control', { recording: !stopping });
  } catch (error) { recordingPending = false; throw error; }
  if (stopping) setTimeout(() => refresh().catch(notice), 900);
});
action('monitor-toggle', () => invoke('monitor_enable', { enabled: !monitorEnabled, endpoint: el('monitor-device').value }));
action('choose-root', async () => { const r = await invoke('choose_folder', { purpose: 'recordings' }); if (r?.config) { applyConfig(r.config); rows = []; selected = undefined; await refresh(); } });
action('open-session', async () => { const r = await invoke('choose_folder', { purpose: 'session' }); if (r?.session) { rows = rows.filter(v => v.directory !== r.session.directory); rows.unshift(r.session); select(r.session); } else if (r?.config) { applyConfig(r.config); await refresh(); } });
action('open-recordings', () => invoke('reveal_recordings_root'));
action('refresh', refresh); action('play-processed', () => play(false)); action('play-original', () => play(true)); action('reveal', () => invoke('reveal_recording', { path: selected.directory }));
action('render', () => invoke('reprocess_recording', { path: selected.directory, options: { gain_db: Number(el('render-gain').value), source_mode: el('source-mode').value } })); action('cancel', () => invoke('cancel_reprocess'));
el('gain').addEventListener('input', () => {
  el('gain-value').value = `${el('gain').value} dB`;
  pendingGain = Number(el('gain').value);
  if (!gainTimer) gainTimer = setTimeout(() => flushGain().catch(notice), 50);
});
el('save-raw').addEventListener('change', updateSaveSummary);
for (const id of ['save-settings', 'reprocess-settings']) {
  const key = `vsm.${id}.open`;
  try { el(id).open = localStorage.getItem(key) === 'true'; } catch { /* Storage may be unavailable. */ }
  el(id).addEventListener('toggle', () => {
    try { localStorage.setItem(key, String(el(id).open)); } catch { /* Keep disclosures usable without storage. */ }
  });
}
el('player').addEventListener('error', () => { if (el('player').src) notice('音声を再生できませんでした。保存場所からWAVを確認できます。'); });
async function initialize() { await poll(); const devices = await invoke('audio_devices'); for (const [key, id] of [['inputs','microphone'], ['outputs','monitor-device']]) { for (const d of devices[key] || []) { const option = document.createElement('option'); option.value = d.endpoint_id || d.id; option.textContent = `${d.name || d.friendly_name}${d.supported === false ? '（形式未対応）' : ''}`; option.disabled = d.supported === false; el(id).append(option); } } applyConfig(state.config); if (devices.message) notice(devices.message); await refresh(); }
initialize().catch(notice).finally(() => { setInterval(() => { if (!busy) poll().catch(notice); }, 700); });
