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
let preferredOriginal = false, preferredVideo = false, playerKey = '', playerGeneration = 0, playerLoad = Promise.resolve();
let videoConfigPending = false, videoSourcePending = false, previewRequest = false, previewGeneration = 0, lastVideoError = '', lastVideoState = '', wasVideoSaving = false;
let videoSources = [];
let workspace = 'audio', captureWorkspace = 'audio', workspaceInitialized = false, workspacePending = false, workspaceGeneration = 0;
const workspaceNames = ['audio', 'video', 'library'];
const workspaceScroll = new Map();
function showWorkspace(name) {
  ++workspaceGeneration;
  if (name !== workspace) {
    workspaceScroll.set(workspace, window.scrollY || 0);
    if (workspace === 'library') {
      // Leaving the library pauses playback without losing the selected take or seek position.
      for (const id of ['player', 'video-player']) el(id).pause();
    }
    clearVideoPreview();
  }
  workspace = name;
  for (const tab of workspaceNames) {
    const selected = tab === name;
    el(`tab-${tab}`).setAttribute('aria-selected', String(selected));
    el(`tab-${tab}`).tabIndex = selected ? 0 : -1;
    el(`${tab}-panel`).hidden = !selected;
  }
  if (name !== 'library') {
    const host = el(`${name}-panel`), live = el('live-panel');
    if (live.parentElement !== host) host.append(live);
    setText('live-title', name === 'video' ? '動画収録' : '音声収録');
  }
  el('video-settings').hidden = name !== 'video';
  try { localStorage.setItem('vsm.workspace', name); } catch { /* Tabs work without storage. */ }
  window.scrollTo?.(0, workspaceScroll.get(name) || 0);
  updateWorkspace();
}
function updateWorkspace() {
  const engine = state?.engine || {}, video = state?.video || {};
  const recording = !!engine.recording || !!video.status?.active;
  const saving = engine.archive_state === 'finalizing' || video.status?.state === 'finalizing' || (!recording && !recordingPending && !!engine.gain_locked);
  const locked = videoLocked() || workspacePending || videoConfigPending || videoSourcePending;
  for (const name of workspaceNames) {
    const tab = el(`tab-${name}`);
    tab.disabled = !workspaceInitialized || (name !== 'library' && (workspacePending || (name !== captureWorkspace && locked)));
    tab.toggleAttribute('data-recording', name === captureWorkspace && recording);
    tab.title = tab.disabled && workspaceInitialized ? '収録・保存が終わると切り替えられます' : '';
  }
  el('capture-strip').hidden = workspace !== 'library' || !(engine.active || recording || saving || recordingPending);
  const obsLinked = state?.obs_link?.status?.state === 'connected' && !video.status?.active;
  setText('capture-summary', saving ? '保存中…' : recording ? (video.status?.active ? '録画中' : obsLinked ? '録音中 · OBSに連動' : '録音中') : recordingPending ? '収録を開始中…' : '入力待機中');
  el('capture-strip').classList.toggle('recording', recording);
  el('capture-stop').hidden = !recording;
  el('capture-stop').disabled = busy || recordingPending || saving || obsLinked;
  setText('capture-stop', video.status?.active ? '録画を停止' : '録音を停止');
}
async function switchWorkspace(name) {
  if (!workspaceNames.includes(name) || !workspaceInitialized || name === workspace) return;
  if (name === 'library' || name === captureWorkspace) { showWorkspace(name); return; }
  if (busy || videoLocked() || videoConfigPending || videoSourcePending) return;
  busy = true;
  workspacePending = true;
  const generation = workspaceGeneration;
  videoConfigPending = true;
  notice('');
  clearVideoPreview();
  el('record-toggle').disabled = true;
  updateWorkspace();
  try {
    await invoke('video_configure', { value: { enabled: name === 'video', sender_name: state.video?.sender_name || '', fps: 30 } });
    captureWorkspace = name;
    if (generation === workspaceGeneration) showWorkspace(name);
    await poll();
  } catch (error) {
    // The previous tab stays selected when a mode change is rejected.
    try { await poll(); } catch { /* Retain the original configuration error. */ }
    notice(error);
  } finally {
    busy = false;
    workspacePending = false;
    videoConfigPending = false;
    updateVideoState();
    updateWorkspace();
  }
}
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
function applyObsSettings(o) { el('obs-enabled').checked = !!o.enabled; el('obs-host').value = o.host; el('obs-port').value = o.port; el('obs-password').value = ''; el('obs-password').placeholder = o.password_set ? '変更しない場合は空欄' : '未設定'; }
const obsLabels = { off: '無効', connecting: '接続中…', connected: '接続済み', auth_failed: 'パスワードが違います', disconnected: '未接続', stopped: '停止' };
function obsText(o) {
  const s = o.status || {};
  let text = obsLabels[s.state] || s.state || '';
  if (s.state === 'connected') text += o.obs_recording ? ' · OBS録画中' : ' · OBS録画待ち';
  if (s.error && s.state !== 'auth_failed') text += `：${s.error}`;
  return text;
}
function videoLocked() {
  const engine = state?.engine || {}, video = state?.video?.status || {};
  return !!engine.recording || !!video.active || !!engine.gain_locked || recordingPending || engine.archive_state === 'finalizing' || video.state === 'finalizing';
}
function selectVideoSender(name) {
  if (name && ![...el('video-source').options].some(option => option.value === name)) {
    const option = document.createElement('option');
    option.value = name;
    option.textContent = name === '__vsm_test_pattern' ? 'テスト映像' : `${name}（未検出）`;
    el('video-source').append(option);
  }
  el('video-source').value = name || '';
}
function updateVideoState() {
  const video = state?.video || {}, status = video.status || {};
  if (!videoConfigPending) {
    el('video-enabled').checked = !!video.enabled;
    selectVideoSender(video.sender_name);
  }
  const enabled = el('video-enabled').checked;
  el('video-options').hidden = !enabled;
  const locked = videoLocked() || videoConfigPending || videoSourcePending;
  for (const id of ['video-enabled', 'video-source', 'video-refresh']) el(id).disabled = locked;
  const waitingError = status.state === 'waiting' ? status.error : !status.active ? video.preview?.error : '';
  const saveError = status.state === 'waiting' ? '' : status.error;
  let text = '', errorMessage = '';
  if (saveError) {
    text = status.state === 'saved' ? `保存済み · 映像取得の警告：${saveError}` : `動画保存に失敗：${saveError}`;
    errorMessage = status.state === 'saved' ? `動画は保存しましたが、映像取得に問題がありました：${saveError}` : `動画保存に失敗しました：${saveError}`;
  } else if (waitingError) {
    text = `映像入力待ち：${waitingError}`;
    errorMessage = `${text}。映像一覧を更新してください。`;
  } else if (status.state === 'finalizing') text = '動画を保存中…';
  else if (status.active) text = '録画中 · 30 fps';
  else if (status.state === 'saved') text = '保存済み · MP4';
  else if (enabled) text = el('video-source').value ? '30 fps · MP4' : '映像を選んでください';
  el('video-state').textContent = text;
  el('video-preview-hint').textContent = waitingError ? '映像入力待ちです。映像一覧を更新してください。' : '映像を待っています…';
  if (!status.active && (video.preview?.active === false || status.state === 'waiting')) clearVideoPreview();
  const errorKey = errorMessage ? `${status.state}:${errorMessage}` : '';
  if (errorKey && errorKey !== lastVideoError) notice(errorMessage);
  lastVideoError = errorKey;
}
async function refreshVideoSources() {
  videoSourcePending = true;
  updateVideoState();
  try {
    const result = await invoke('video_sources');
    const sender = el('video-source').value || state?.video?.sender_name || '';
    videoSources = result.sources || [];
    el('video-source').replaceChildren(new Option('映像を選んでください', ''));
    for (const source of videoSources) {
      const size = source.width && source.height ? ` · ${source.width}×${source.height}` : '';
      el('video-source').append(new Option(`${source.name}${size}`, source.name));
    }
    if (result.synthetic_allowed) el('video-source').append(new Option('テスト映像', '__vsm_test_pattern'));
    selectVideoSender(sender);
  } finally { videoSourcePending = false; updateVideoState(); }
}
async function configureVideo() {
  if (busy || videoLocked()) { updateVideoState(); return; }
  busy = true;
  videoConfigPending = true;
  clearVideoPreview();
  notice('');
  updateVideoState();
  try {
    await invoke('video_configure', { value: { enabled: el('video-enabled').checked, sender_name: el('video-source').value, fps: 30 } });
    await poll();
  } catch (error) { notice(error); }
  finally { busy = false; videoConfigPending = false; updateVideoState(); }
}
function clearVideoPreview() {
  ++previewGeneration;
  el('video-preview').hidden = true;
  el('video-preview').removeAttribute('src');
  el('video-preview-hint').hidden = false;
}
async function pollVideoPreview() {
  if (document.hidden || workspace !== 'video' || !el('video-preview-settings').open || !state?.video?.enabled || !state?.video?.sender_name || videoConfigPending || (!state?.video?.status?.active && (state?.video?.preview?.active === false || state?.video?.status?.state === 'waiting'))) {
    clearVideoPreview();
    return;
  }
  if (previewRequest) return;
  previewRequest = true;
  const generation = previewGeneration;
  try {
    const preview = await invoke('video_preview');
    if (generation !== previewGeneration) return;
    if (!preview?.bmp_base64) { clearVideoPreview(); return; }
    el('video-preview').src = `data:image/bmp;base64,${preview.bmp_base64}`;
    el('video-preview').hidden = false;
    el('video-preview-hint').hidden = true;
  } catch { /* Recording status reports capture errors; preview is optional. */ }
  finally { previewRequest = false; }
}
setInterval(pollVideoPreview, 500);
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
function select(row) {
  if (renaming && recordingKey(renaming) !== recordingKey(row.directory)) closeRename();
  if (recordingKey(selected?.directory) !== recordingKey(row.directory)) preferredVideo = !!row.video_path;
  selected = row;
  el('selected-name').textContent = row.name || '録音';
  el('selected-info').textContent = row.message || '';
  el('play-video').hidden = !row.video_path;
  el('play-video').disabled = !row.video_path;
  el('play-processed').disabled = !row.audio_path;
  el('play-original').disabled = !row.raw_saved;
  el('reveal').disabled = false;
  el('render').disabled = !row.can_reprocess || !!state?.render.active;
  el('render-gain').value = row.config?.gain_db ?? -18;
  el('rename').disabled = !canRename();
  renderList();
  syncPlayer().catch(notice);
}
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
    badge.textContent = row.video_path ? '動画' : row.can_reprocess ? 'RAW+VSM' : 'VSM';
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
  for (const id of ['player', 'video-player']) {
    el(id).pause();
    el(id).removeAttribute('src');
    el(id).load();
  }
  el('player').hidden = false;
  el('video-player').hidden = true;
  el('playing').textContent = '';
  for (const id of ['play-video', 'play-processed', 'play-original']) {
    el(id).setAttribute('aria-pressed', 'false');
    el(id).classList.remove('btn-primary');
    el(id).classList.add('btn-secondary');
  }
}
async function syncPlayer(autoplay = false) {
  const row = selected;
  if (!row?.video_path && !row?.audio_path && !row?.raw_saved) {
    clearPlayer();
    el('playing').textContent = row ? '再生できるファイルがありません' : '';
    return;
  }
  const video = !!row.video_path && (preferredVideo || (!row.audio_path && !row.raw_saved));
  const original = !video && row.raw_saved && (preferredOriginal || !row.audio_path);
  const kind = video ? 'video' : original ? 'original' : 'processed';
  const player = el(video ? 'video-player' : 'player');
  const key = JSON.stringify([recordingKey(row.directory), kind, video ? row.video_path : original ? '' : row.audio_path]);
  // Refreshing the list must not reset playback or its seek position.
  if (key !== playerKey) {
    clearPlayer();
    playerKey = key;
    const generation = playerGeneration;
    playerLoad = (async () => {
      try {
        const path = video
          ? await invoke('recording_video', { path: row.directory })
          : await invoke('recording_audio', { path: row.directory, original: !!original });
        // A slower previous selection must never replace the current recording.
        if (generation !== playerGeneration) return;
        player.src = convertFileSrc(path);
        player.load();
        el('player').hidden = video;
        el('video-player').hidden = !video;
        el('playing').textContent = video ? '動画 · 処理済み音声' : original ? '原音 (RAW)' : '処理済み音声';
        const id = video ? 'play-video' : original ? 'play-original' : 'play-processed';
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
  if (autoplay && workspace === 'library' && generation === playerGeneration && playerKey === key && player.getAttribute('src')) {
    try { await player.play(); }
    catch (error) { if (generation === playerGeneration) throw error; }
  }
}
async function play(original) { preferredVideo = false; preferredOriginal = original; await syncPlayer(true); }
async function playVideo() { preferredVideo = true; await syncPlayer(true); }
async function poll() {
  state = await invoke('studio_status');
  const e = state.engine, active = !!e.active, recording = !!e.recording || !!state.video?.status?.active;
  if (!workspaceInitialized) {
    captureWorkspace = state.video?.enabled ? 'video' : 'audio';
    workspaceInitialized = true;
    let saved;
    try { saved = localStorage.getItem('vsm.workspace'); } catch { /* Use the current capture mode. */ }
    showWorkspace(saved === 'library' ? 'library' : captureWorkspace);
  } else if (!workspacePending) {
    captureWorkspace = state.video?.enabled ? 'video' : 'audio';
    if (workspace !== 'library' && workspace !== captureWorkspace) showWorkspace(captureWorkspace);
  }
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
  const obs = state.obs_link, obsLinked = obs.status?.state === 'connected';
  updateVideoState();
  const video = state.video || {}, finalizing = video.status?.state === 'finalizing' || e.archive_state === 'finalizing' || (!recording && !recordingPending && !!e.gain_locked);
  el('record-toggle').disabled = videoConfigPending || workspacePending || !active || (obsLinked && !video.status?.active) || recordingPending || finalizing || (!recording && !!e.gain_locked) || (!recording && video.enabled && !video.sender_name);
  setText('record-toggle', video.enabled ? (recording ? '録画を停止' : '録画を開始') : (recording ? '録音を停止' : '録音を開始'));
  el('record-toggle').classList.toggle('recording', recording);
  el('record-state').textContent = finalizing ? '保存中…' : recording ? (video.status?.active ? '録画中' : obsLinked ? '録音中 · OBSの録画に連動' : '録音中') : active ? (obsLinked ? 'OBSの録画開始を待っています · 直前の音声も保持しています' : '待機中 · 直前の音声も保持しています') : '';
  el('obs-state').textContent = obsText(obs);
  el('obs-summary').textContent = obs.enabled ? obsText(obs) : '無効';
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
  const videoFinished = wasVideoSaving && !video.status?.active && !finalizing;
  const videoSaved = video.status?.state === 'saved' && lastVideoState !== 'saved';
  if ((wasRendering && !r.active) || archive !== lastArchive || videoFinished || videoSaved) {
    lastArchive = archive;
    await refresh();
  }
  wasRendering = !!r.active;
  wasVideoSaving = !!video.status?.active || finalizing;
  lastVideoState = video.status?.state || '';
  updateWorkspace();
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
async function toggleRecording() {
  const stopping = !!state.engine.recording || !!state.video?.status?.active;
  recordingPending = !stopping;
  el('gain').disabled = true;
  for (const id of ['video-enabled', 'video-source', 'video-refresh']) el(id).disabled = true;
  try {
    await flushGain();
    await invoke('record_control', { recording: !stopping });
  } catch (error) { recordingPending = false; throw error; }
  if (stopping) setTimeout(() => refresh().catch(notice), 900);
}
action('record-toggle', toggleRecording);
action('capture-stop', async () => { if (state?.engine.recording || state?.video?.status?.active) await toggleRecording(); });
el('capture-return').addEventListener('click', () => switchWorkspace(captureWorkspace));
for (const name of workspaceNames) {
  el(`tab-${name}`).addEventListener('click', () => switchWorkspace(name));
  el(`tab-${name}`).addEventListener('keydown', async event => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const available = workspaceNames.filter(value => !el(`tab-${value}`).disabled);
    const index = available.indexOf(name);
    const next = event.key === 'Home' ? available[0] : event.key === 'End' ? available.at(-1) : available[(index + (event.key === 'ArrowRight' ? 1 : -1) + available.length) % available.length];
    if (!next) return;
    el(`tab-${next}`).focus();
    await switchWorkspace(next);
    if (workspace === next) el(`tab-${next}`).focus();
  });
}
action('video-refresh' , refreshVideoSources);
el('video-enabled').addEventListener('change', configureVideo);
el('video-source').addEventListener('change', configureVideo);
el('video-preview-settings').addEventListener('toggle', () => { if (el('video-preview-settings').open) pollVideoPreview(); else clearVideoPreview(); });
action('obs-save', async () => {
  await invoke('obs_link_configure', { value: { enabled: el('obs-enabled').checked, host: el('obs-host').value, port: Number(el('obs-port').value), password: el('obs-password').value } });
  applyObsSettings((await invoke('studio_status')).obs_link);
});
action('monitor-toggle', () => invoke('monitor_enable', { enabled: !monitorEnabled, endpoint: el('monitor-device').value }));
action('choose-root', async () => { const r = await invoke('choose_folder', { purpose: 'recordings' }); if (r?.config) { applyConfig(r.config); rows = []; selected = undefined; await refresh(); } });
action('open-session', async () => { const r = await invoke('choose_folder', { purpose: 'session' }); if (r?.session) { rows = rows.filter(v => v.directory !== r.session.directory); rows.unshift(r.session); select(r.session); } else if (r?.config) { applyConfig(r.config); await refresh(); } });
action('open-recordings', () => invoke('reveal_recordings_root'));
action('refresh', refresh); action('play-video', playVideo); action('play-processed', () => play(false)); action('play-original', () => play(true)); action('reveal', () => invoke('reveal_recording', { path: selected.directory }));
action('render', () => invoke('reprocess_recording', { path: selected.directory, options: { gain_db: Number(el('render-gain').value), source_mode: el('source-mode').value } })); action('cancel', () => invoke('cancel_reprocess'));
el('gain').addEventListener('input', () => {
  el('gain-value').value = `${el('gain').value} dB`;
  pendingGain = Number(el('gain').value);
  if (!gainTimer) gainTimer = setTimeout(() => flushGain().catch(notice), 50);
});
el('save-raw').addEventListener('change', updateSaveSummary);
for (const id of ['save-settings', 'obs-settings', 'reprocess-settings', 'video-preview-settings']) {
  const key = `vsm.${id}.open`;
  try { el(id).open = localStorage.getItem(key) === 'true'; } catch { /* Storage may be unavailable. */ }
  el(id).addEventListener('toggle', () => {
    try { localStorage.setItem(key, String(el(id).open)); } catch { /* Keep disclosures usable without storage. */ }
  });
}
el('player').addEventListener('error', () => { if (el('player').getAttribute('src')) notice('音声を再生できませんでした。保存場所からWAVを確認できます。'); });
el('video-player').addEventListener('error', () => { if (el('video-player').getAttribute('src')) notice('動画を再生できませんでした。保存場所からMP4を確認できます。'); });
async function initialize() { await poll(); const devices = await invoke('audio_devices'); for (const [key, id] of [['inputs','microphone'], ['outputs','monitor-device']]) { for (const d of devices[key] || []) { const option = document.createElement('option'); option.value = d.endpoint_id || d.id; option.textContent = `${d.name || d.friendly_name}${d.supported === false ? '（形式未対応）' : ''}`; option.disabled = d.supported === false; el(id).append(option); } } applyConfig(state.config); applyObsSettings(state.obs_link); await refreshVideoSources(); if (devices.message) notice(devices.message); await refresh(); }
initialize().catch(notice).finally(() => { setInterval(() => { if (!busy) poll().catch(notice); }, 700); });
