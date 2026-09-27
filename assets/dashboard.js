(() => {
  const TOKEN_KEY = 'apollo-dashboard-token';
  const LAST_GUILD_KEY = 'apollo-dashboard-last-guild';
  const LAST_CHANNEL_KEY_PREFIX = 'apollo-dashboard-last-channel-';
  function lastChannelKey(guildId) {
    return LAST_CHANNEL_KEY_PREFIX + guildId;
  }
  let token = localStorage.getItem(TOKEN_KEY);
  let currentGuildId = null;
  let guilds = [];
  let socket = null;
  const SOCKET_RETRY_MIN_MS = 1000;
  const SOCKET_RETRY_MAX_MS = 10000;
  // Three missed pushes of the 1.5 s snapshot feed.
  const SNAPSHOT_STALE_MS = 5000;
  const STALE_STATUS_MESSAGE = 'No update from the bot for a few seconds — waiting for it to answer.';
  let socketRetryTimer = null;
  let socketRetryDelay = SOCKET_RETRY_MIN_MS;
  let staleTimer = null;
  let progressFrozen = false;
  let volumeDebounce = null;
  let searchDebounce = null;
  let searchSeq = 0;
  let searchActiveIndex = -1;
  let lastSeenPlayingVideoId = null;
  let statusDismissTimeout = null;
  let npCrossfadeTimeout = null;
  // True while a queue row is being dragged for reorder — while set,
  // renderQueue must not let the live snapshot feed reconcile row DOM/order
  // out from under the user's cursor.
  let queueDragActive = false;

  const loginScreen = document.getElementById('login-screen');
  const dashboard = document.getElementById('dashboard');
  const loginError = document.getElementById('login-error');
  const statusLine = document.getElementById('status-line');


  // Shows a status message. Non-error messages fade in and auto-dismiss after
  // a few seconds; error messages stay up until the next status change so
  // they actually get read. Any pending auto-dismiss is cleared whenever a
  // new message arrives, so rapid actions don't cut each other off mid-fade.
  function showStatus(message, options = {}) {
    const isError = !!options.isError;
    clearTimeout(statusDismissTimeout);
    statusDismissTimeout = null;
    statusLine.textContent = message || '';
    statusLine.classList.toggle('status-error', isError);
    statusLine.classList.toggle('status-visible', !!message);
    if (message && !isError) {
      statusDismissTimeout = setTimeout(() => {
        statusLine.classList.remove('status-visible');
        statusDismissTimeout = null;
      }, 4000);
    }
  }

  // Swaps a button's label for a spinner while an action it triggered is
  // in flight, so a click gets an immediate visual response instead of
  // nothing happening until the request resolves.
  function setButtonBusy(btn, busy) {
    if (!btn) return;
    btn.disabled = busy;
    btn.classList.toggle('btn-busy', busy);
  }

  function showDashboard() {
    loginScreen.style.display = 'none';
    dashboard.style.display = 'flex';
    loadGuilds();
    loadCurrentUser();
  }

  function showLogin(message) {
    token = null;
    localStorage.removeItem(TOKEN_KEY);
    closeSocket();
    setTabIcon(false);
    loginScreen.style.display = 'block';
    dashboard.style.display = 'none';
    loginError.textContent = message || '';
    currentUser = null;
    navUsersBtn.style.display = 'none';
    showPage('dashboard');
  }

  async function api(path, options = {}) {
    const response = await fetch(path, {
      ...options,
      headers: { ...(options.headers || {}), Authorization: `Bearer ${token}` },
    });
    if (response.status === 401) {
      showLogin('Session expired — please sign in again.');
      throw new Error('unauthorized');
    }
    return response;
  }

  async function attemptLogin() {
    const username = document.getElementById('username').value.trim();
    const password = document.getElementById('password').value;
    loginError.textContent = '';
    try {
      const response = await fetch('/api/login', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ username, password }),
      });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        loginError.textContent = body.error || 'Sign-in failed.';
        return;
      }
      const body = await response.json();
      token = body.token;
      localStorage.setItem(TOKEN_KEY, token);
      showDashboard();
    } catch (err) {
      loginError.textContent = 'Could not reach the server.';
    }
  }

  document.getElementById('login-btn').addEventListener('click', attemptLogin);
  for (const fieldId of ['username', 'password']) {
    document.getElementById(fieldId).addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.keyCode === 13) attemptLogin();
    });
  }

  document.getElementById('logout-btn').addEventListener('click', () => {
    fetch('/api/logout', { method: 'POST', headers: { Authorization: `Bearer ${token}` } }).catch(() => {});
    showLogin();
  });

  // ---- Deterministic per-id color, used for server avatars, playlist
  // covers and search/queue thumbnails (the backend has no real artwork). ----
  function hueFromId(id) {
    let hash = 0;
    for (let i = 0; i < id.length; i++) {
      hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
    }
    return hash % 360;
  }

  function hueGradient(hue) {
    const hue2 = (hue + 40) % 360;
    return `linear-gradient(135deg, hsl(${hue}, 65%, 55%), hsl(${hue2}, 70%, 45%))`;
  }

  // Lays the gradient down first (so it's there instantly and stays as the
  // fallback), then loads a real YouTube thumbnail over it in the background
  // — dropping the img on error/no-id leaves the gradient showing through.
  function applyThumbnail(el, videoId, gradientSeed) {
    el.style.background = hueGradient(hueFromId(gradientSeed));
    el.innerHTML = '';
    if (!videoId) return;
    const img = document.createElement('img');
    img.src = `https://i.ytimg.com/vi/${videoId}/mqdefault.jpg`;
    img.alt = '';
    img.loading = 'lazy';
    img.addEventListener('error', () => img.remove());
    el.appendChild(img);
  }

  // ---- Server switcher ----
  const serverSwitcher = document.getElementById('server-switcher');
  const serverMenu = document.getElementById('server-menu');
  const serverBtn = document.getElementById('server-btn');
  const serverAvatar = document.getElementById('server-avatar');
  const serverName = document.getElementById('server-name');

  function closeServerMenu() {
    serverMenu.classList.remove('open');
  }

  serverBtn.addEventListener('click', (event) => {
    event.stopPropagation();
    serverMenu.classList.toggle('open');
  });

  document.addEventListener('click', (event) => {
    if (!serverSwitcher.contains(event.target)) closeServerMenu();
    if (!topbarSearch.contains(event.target)) closeSearchDropdown();
  });

  function renderServerMenu() {
    serverMenu.innerHTML = '';
    for (const guild of guilds) {
      const row = document.createElement('button');
      row.type = 'button';
      row.className = 'server-row flex items-center gap-3 w-full h-10 px-2.5 rounded text-left text-base text-white hover:bg-hover' + (guild.id === currentGuildId ? ' active' : '');

      const avatar = document.createElement('span');
      avatar.className = 'flex items-center justify-center size-6 rounded-full shrink-0 text-xs font-bold text-black';
      avatar.style.background = hueGradient(hueFromId(guild.id));
      avatar.textContent = guild.name.charAt(0).toUpperCase();
      row.appendChild(avatar);

      const name = document.createElement('span');
      name.className = 'truncate';
      name.textContent = guild.name;
      row.appendChild(name);

      row.addEventListener('click', () => {
        selectGuild(guild.id);
        closeServerMenu();
      });
      serverMenu.appendChild(row);
    }
  }

  function updateServerButton(guild) {
    serverAvatar.style.background = hueGradient(hueFromId(guild.id));
    serverAvatar.textContent = guild.name.charAt(0).toUpperCase();
    serverName.textContent = guild.name;
  }

  async function loadGuilds() {
    const response = await api('/api/guilds');
    guilds = await response.json();
    if (guilds.length > 0) {
      renderServerMenu();
      const lastGuildId = localStorage.getItem(LAST_GUILD_KEY);
      const stillPresent = lastGuildId && guilds.some((g) => g.id === lastGuildId);
      selectGuild(stillPresent ? lastGuildId : guilds[0].id);
    } else {
      showStatus('Apollo is not in any servers yet.');
    }
  }

  function selectGuild(guildId) {
    currentGuildId = guildId;
    localStorage.setItem(LAST_GUILD_KEY, guildId);
    const guild = guilds.find((g) => g.id === guildId);
    if (guild) updateServerButton(guild);
    renderServerMenu();
    closeSocket();
    connectSocket(guildId);
    clearSearch();
    closeJoinModal();
    loadPlaylists(guildId);
    loadFavourites(guildId);
  }

  // Tears the feed down completely: the socket, any pending reconnect and
  // the stale-feed timer, so nothing fires again until the next connect.
  function closeSocket() {
    clearTimeout(socketRetryTimer);
    socketRetryTimer = null;
    clearTimeout(staleTimer);
    staleTimer = null;
    if (socket) { socket.close(); socket = null; }
    setSnapshotStale(false);
  }

  // Replaces the socket only: the stale-feed timer keeps running across
  // reconnect attempts, so an outage is flagged even while the feed is
  // still trying to come back.
  function connectSocket(guildId) {
    clearTimeout(socketRetryTimer);
    socketRetryTimer = null;
    if (socket) { socket.close(); socket = null; }
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const ws = new WebSocket(`${protocol}//${location.host}/api/guilds/${guildId}/ws?token=${encodeURIComponent(token)}`);
    socket = ws;
    let opened = false;
    ws.addEventListener('open', () => { opened = true; socketRetryDelay = SOCKET_RETRY_MIN_MS; });
    ws.addEventListener('message', (event) => {
      try {
        renderSnapshot(JSON.parse(event.data));
      } catch (err) { /* ignore malformed frame */ }
    });
    // A feed that drops (bot restart, network blip) comes back on its own
    // with a growing delay, unless this socket was already replaced or the
    // user signed out in the meantime.
    ws.addEventListener('close', () => {
      if (socket !== ws) return;
      socket = null;
      if (!token || currentGuildId !== guildId) return;
      if (!staleTimer) staleTimer = setTimeout(() => setSnapshotStale(true), SNAPSHOT_STALE_MS);
      socketRetryTimer = setTimeout(() => retrySocket(guildId, opened), socketRetryDelay);
      socketRetryDelay = Math.min(socketRetryDelay * 2, SOCKET_RETRY_MAX_MS);
    });
  }

  // A handshake the server refuses only shows up as a close, so a socket
  // that never opened is preceded by a plain request: a revoked session
  // lands on the sign-in screen there instead of retrying forever, while
  // a bot that is merely down keeps the retries going.
  async function retrySocket(guildId, hadOpened) {
    socketRetryTimer = null;
    if (!hadOpened) {
      try {
        await api('/api/guilds');
      } catch (err) {
        if (err.message === 'unauthorized') return;
      }
    }
    if (!token || currentGuildId !== guildId || socket) return;
    connectSocket(guildId);
  }

  // Flags the now-playing card as running on stale data once the snapshot
  // feed goes quiet, and pins the progress bar where it is so it stops
  // sailing on to 100% with nothing behind it.
  function setSnapshotStale(stale) {
    if (stale === dashboard.classList.contains('snapshot-stale')) return;
    dashboard.classList.toggle('snapshot-stale', stale);
    if (stale) {
      freezeProgress();
      // The feed has stopped saying anything about playback, so the tab
      // icon stops claiming it.
      setTabIcon(false);
      showStatus(STALE_STATUS_MESSAGE, { isError: true });
    } else if (statusLine.textContent === STALE_STATUS_MESSAGE) {
      showStatus('');
    }
  }

  // Reading the computed width forces a layout, so the bar is only pinned
  // once per outage; the next real position unfreezes it.
  function freezeProgress() {
    if (progressFrozen) return;
    progressFrozen = true;
    const fill = document.getElementById('np-progress-fill');
    const width = getComputedStyle(fill).width;
    fill.style.transition = 'none';
    fill.style.width = width;
  }

  const STATE_LABELS = {
    playing: 'NOW PLAYING',
    paused: 'PAUSED',
    buffering: 'BUFFERING…',
    queue_finished: 'QUEUE FINISHED',
    empty: 'NOTHING PLAYING',
  };

  function formatDuration(totalSeconds) {
    if (totalSeconds == null) return '';
    const m = Math.floor(totalSeconds / 60);
    const s = Math.floor(totalSeconds % 60);
    return `${m}:${String(s).padStart(2, '0')}`;
  }

  const HERO_STATES = new Set(['playing', 'paused', 'buffering']);

  function renderHeroOrEmpty(snapshot) {
    const showHero = HERO_STATES.has(snapshot.state);
    document.getElementById('np-hero').style.display = showHero ? 'flex' : 'none';
    document.getElementById('np-empty').style.display = showHero ? 'none' : 'flex';
    if (showHero) return;

    if (snapshot.state === 'queue_finished' && snapshot.track) {
      document.getElementById('np-empty-heading').textContent = 'Queue finished';
      document.getElementById('np-empty-subtext').textContent =
        `"${snapshot.track.title}" was the last track. Search or play a playlist to keep going.`;
    } else {
      document.getElementById('np-empty-heading').textContent = 'Nothing is playing';
      document.getElementById('np-empty-subtext').textContent =
        'Search YouTube above or play a playlist to get started.';
    }
  }

  // ---- Keyed list reconciliation ----
  // Diffs `items` against the container's current children (matched by
  // dataset.key) instead of wiping and rebuilding the list. Persisting rows
  // are reused in place (so hover/focus state and CSS transitions survive),
  // new rows are created and enter-animated, and rows no longer present are
  // exit-animated before being removed. Duplicate keys are matched in order
  // via a Map<key, Element[]> so two same-keyed items don't collapse onto a
  // single DOM node.
  function animateEnter(el) {
    el.classList.add('row-enter');
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        el.classList.remove('row-enter');
      });
    });
  }

  function animateExit(el, onDone) {
    let done = false;
    let fallback = null;
    function finish() {
      if (done) return;
      done = true;
      clearTimeout(fallback);
      el.removeEventListener('transitionend', onTransitionEnd);
      onDone();
    }
    function onTransitionEnd(event) {
      if (event.target !== el) return;
      finish();
    }
    el.addEventListener('transitionend', onTransitionEnd);
    fallback = setTimeout(finish, 250);
    // The exit transition shrinks max-height to 0, which needs an explicit
    // starting px value to animate from (CSS can't interpolate from the
    // resting `none`) — measure the row's actual rendered height rather than
    // guessing a constant, so rows taller than an assumed height never get
    // clipped in the resting (non-exiting) state. The inline value is only
    // there to seed that starting point for one committed frame: it's
    // cleared in the same breath `row-exit` is added so the class's own
    // `max-height: 0` (not an inline style that would outrank it) drives the
    // actual animated transition.
    el.style.maxHeight = `${el.scrollHeight}px`;
    void el.offsetHeight;
    el.classList.add('row-exit');
    el.style.maxHeight = '';
  }

  function reconcileList(container, items, keyOf, createRow, updateRow) {
    const existingByKey = new Map();
    for (const el of Array.from(container.children)) {
      if (el.dataset.exiting === 'true') continue;
      const key = el.dataset.key;
      if (key == null) continue;
      let bucket = existingByKey.get(key);
      if (!bucket) { bucket = []; existingByKey.set(key, bucket); }
      bucket.push(el);
    }

    const usedElements = new Set();
    let prevSibling = null;

    items.forEach((item, index) => {
      const key = String(keyOf(item));
      const bucket = existingByKey.get(key);
      let el = bucket && bucket.length ? bucket.shift() : null;

      if (el) {
        updateRow(el, item, index, items);
        if (prevSibling === null) {
          if (container.firstChild !== el) container.insertBefore(el, container.firstChild);
        } else if (prevSibling.nextSibling !== el) {
          container.insertBefore(el, prevSibling.nextSibling);
        }
      } else {
        el = createRow(item, index, items);
        el.dataset.key = key;
        if (prevSibling === null) {
          container.insertBefore(el, container.firstChild);
        } else {
          container.insertBefore(el, prevSibling.nextSibling);
        }
        animateEnter(el);
      }
      usedElements.add(el);
      prevSibling = el;
    });

    for (const el of Array.from(container.children)) {
      if (usedElements.has(el) || el.dataset.exiting === 'true') continue;
      el.dataset.exiting = 'true';
      animateExit(el, () => {
        if (el.parentNode === container) container.removeChild(el);
      });
    }
  }

  // A cheap fingerprint of the fields updateTrackRowContent writes to the
  // DOM, so a reconcile can tell whether a persisting row's content actually
  // needs touching — most snapshot ticks change only playback position/state,
  // not the queue's track list, so this turns the common case into a string
  // comparison instead of 3 querySelector + 3 textContent writes per row.
  function trackContentSig(track) {
    return `${track.title} ${track.channel} ${track.duration_secs}`;
  }

  // Builds the shared thumb + title/channel + duration layout used by both
  // the search results list and the queue list; callers append their own
  // trailing action button(s).
  function createTrackRow(track) {
    const row = document.createElement('div');
    row.className = 'compact-row relative flex items-center gap-3 min-h-14 p-2 rounded shrink-0 overflow-hidden hover:bg-hover';

    const thumb = document.createElement('div');
    thumb.className = 'compact-thumb thumb relative size-10 rounded shrink-0 overflow-hidden';
    applyThumbnail(thumb, track.video_id, track.video_id);
    row.appendChild(thumb);

    const meta = document.createElement('div');
    meta.className = 'compact-meta min-w-0 flex-1 flex flex-col gap-0.5';
    const titleLine = document.createElement('div');
    titleLine.className = 'flex items-center gap-1.5 min-w-0';
    const titleSpan = document.createElement('span');
    titleSpan.className = 'compact-title text-base truncate';
    titleSpan.textContent = track.title;
    titleLine.appendChild(titleSpan);
    meta.appendChild(titleLine);
    const channel = document.createElement('div');
    channel.className = 'compact-channel text-sm text-secondary truncate';
    channel.textContent = track.channel;
    meta.appendChild(channel);
    row.appendChild(meta);

    const duration = document.createElement('span');
    duration.className = 'compact-duration shrink-0 text-sm text-secondary tabular-nums';
    duration.textContent = formatDuration(track.duration_secs);
    row.appendChild(duration);

    row.dataset.contentSig = trackContentSig(track);
    return row;
  }

  // Mutates an existing row's title/channel/duration in place, without
  // recreating the element — used by every list's updateRow so persisting
  // rows keep their identity across reconciles. The thumbnail is deliberately
  // left untouched: reconcileList only reuses a row when the item's key
  // (video_id) matches, so a reused row's thumbnail is already correct —
  // reapplying it would tear down and recreate the <img> (forcing a reload)
  // on every reconcile, which gets very expensive with large lists like a
  // multi-thousand-track queue. Same reasoning for the contentSig guard below:
  // with a large queue, most reconciles change no track's title/channel/
  // duration at all (only playback position/state), so skipping the DOM
  // writes entirely when the fingerprint matches avoids thousands of
  // pointless querySelector/textContent calls every snapshot tick.
  function updateTrackRowContent(row, track) {
    const sig = trackContentSig(track);
    if (row.dataset.contentSig === sig) return;
    row.dataset.contentSig = sig;
    row.querySelector('.compact-title').textContent = track.title;
    row.querySelector('.compact-channel').textContent = track.channel;
    row.querySelector('.compact-duration').textContent = formatDuration(track.duration_secs);
  }

  const COVER_PLAY_ICON = '<svg width="36" height="36" viewBox="0 0 24 24" fill="currentColor"><polygon points="7,3 22,12 7,21"/></svg>';
  const REMOVE_ICON = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>';
  const PLAY_ICON = '<svg width="13" height="13" viewBox="0 0 24 24" fill="currentColor"><polygon points="6,3 21,12 6,21"/></svg>';
  const GRIP_ICON = '<svg width="10" height="16" viewBox="0 0 10 16" fill="currentColor"><circle cx="2.5" cy="2.5" r="1.5"/><circle cx="7.5" cy="2.5" r="1.5"/><circle cx="2.5" cy="8" r="1.5"/><circle cx="7.5" cy="8" r="1.5"/><circle cx="2.5" cy="13.5" r="1.5"/><circle cx="7.5" cy="13.5" r="1.5"/></svg>';
  const REFRESH_ICON = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-2.64-6.36"/><polyline points="21 3 21 9 15 9"/></svg>';
  const EDIT_ICON = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>';
  const USER_ICON = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/></svg>';
  const SERVER_ICON = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="7" rx="2"/><rect x="2" y="14" width="20" height="7" rx="2"/><line x1="6" y1="6.5" x2="6.01" y2="6.5"/><line x1="6" y1="17.5" x2="6.01" y2="17.5"/></svg>';

  // The play glyph that a clickable thumbnail (a playlist cover, a most
  // played row) shows over its artwork on hover.
  function makeCoverPlayGlyph() {
    const glyph = document.createElement('span');
    glyph.className = 'cover-play absolute inset-0 flex items-center justify-center';
    glyph.innerHTML = COVER_PLAY_ICON;
    return glyph;
  }

  function makeMiniIconBtn(iconSvg, title, disabled, danger) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = danger ? 'mini-icon-btn danger' : 'mini-icon-btn';
    btn.title = title;
    btn.disabled = !!disabled;
    btn.innerHTML = iconSvg;
    return btn;
  }

  // ---- Queue drag-to-reorder ----
  // Pointer-events-based drag (not native HTML5 DnD, whose default ghost
  // image feels clunky). The dragged row stays in normal flow and is offset
  // with an inline `transform: translateY(...)`; siblings that get displaced
  // by a reorder mid-drag are animated into their new slot with a FLIP
  // transition (capture old position, move, capture new position, animate
  // the difference away).
  const queueList = document.getElementById('queue-list');
  let dragState = null;

  function getQueueRows() {
    return Array.from(queueList.children).filter((el) => el.dataset.exiting !== 'true');
  }

  // FLIP-animates `el` from its previous position (expressed as `deltaY`, the
  // signed distance from where it used to be to where it now is) into its
  // current position. Cancels and replaces any FLIP transition already in
  // flight on the same element so rapid successive reorders within one drag
  // don't leave a stale transitionend/timeout pair clobbering a newer one.
  function flipTransition(el, deltaY) {
    if (el._flipCleanup) el._flipCleanup();
    el.style.transition = 'none';
    el.style.transform = `translateY(${deltaY}px)`;
    void el.offsetHeight; // force reflow before re-enabling the transition
    el.style.transition = 'transform 150ms ease';
    el.style.transform = '';
    let done = false;
    let fallback = null;
    function finish() {
      if (done) return;
      done = true;
      clearTimeout(fallback);
      el.removeEventListener('transitionend', onTransitionEnd);
      el.style.transition = '';
      if (el._flipCleanup === finish) el._flipCleanup = null;
    }
    function onTransitionEnd(event) {
      if (event.target !== el) return;
      finish();
    }
    el.addEventListener('transitionend', onTransitionEnd);
    fallback = setTimeout(finish, 250);
    el._flipCleanup = finish;
  }

  // Moves the dragged row to sit before `others[newIndex]` (or at the end),
  // FLIP-animating every other row whose layout position changes as a
  // result, and updates `state.layoutTopDelta` so the caller can keep the
  // dragged row's rendered position under the cursor with no visual jump.
  function reorderDom(state, others, newIndex) {
    const { row, startRect } = state;
    const rows = getQueueRows();
    const beforeRects = new Map();
    for (const r of rows) {
      if (r === row) continue;
      beforeRects.set(r, r.getBoundingClientRect());
    }

    const refRow = others[newIndex] || null;
    row.style.transform = ''; // measure true layout position, ignoring the drag offset
    if (refRow) {
      queueList.insertBefore(row, refRow);
    } else {
      queueList.appendChild(row);
    }
    const layoutTop = row.getBoundingClientRect().top;
    state.layoutTopDelta = layoutTop - startRect.top;

    for (const r of rows) {
      if (r === row) continue;
      const before = beforeRects.get(r);
      const after = r.getBoundingClientRect();
      const diff = before.top - after.top;
      if (diff !== 0) flipTransition(r, diff);
    }
  }

  function onDragPointerMove(event) {
    if (!dragState || event.pointerId !== dragState.pointerId) return;
    const { row, startRect, startClientY } = dragState;
    const deltaY = event.clientY - startClientY;

    const rows = getQueueRows();
    const others = rows.filter((r) => r !== row);
    // The now-playing track (if any) is pinned as the first row but isn't
    // part of the movable queue — its lack of a drag handle already stops it
    // from being dragged itself, but without this floor a dragged row could
    // still be spliced in ahead of it, implying a reorder that can't happen.
    const minIndex = others[0] && others[0].classList.contains('now-playing-row') ? 1 : 0;
    const draggedCenter = startRect.top + deltaY + startRect.height / 2;

    // Walk rows in DOM order summing offsetHeight (a pure layout dimension,
    // never affected by a transform or an in-flight transition on one) to
    // find each sibling's settled position. getBoundingClientRect() would be
    // wrong here: a sibling displaced by the last reorderDom is still mid
    // FLIP-transition (see flipTransition), so its rect lags behind where
    // it's actually headed — if the pointer moves fast enough to sample again
    // before that 150ms transition finishes, the threshold check below would
    // compare against the stale, still-animating position and the dragged
    // row would stop advancing, i.e. appear to "stick" mid-drag.
    const gap = parseFloat(getComputedStyle(queueList).rowGap) || 0;
    let top = queueList.getBoundingClientRect().top - queueList.scrollTop;
    let newIndex = others.length;
    let otherIndex = 0;
    for (const r of rows) {
      const height = r.offsetHeight;
      if (r !== row) {
        const mid = top + height / 2;
        if (draggedCenter < mid) { newIndex = otherIndex; break; }
        otherIndex++;
      }
      top += height + gap;
    }
    if (newIndex < minIndex) newIndex = minIndex;

    if (newIndex !== dragState.tentativeIndex) {
      reorderDom(dragState, others, newIndex);
      dragState.tentativeIndex = newIndex;
    }

    row.style.transform = `translateY(${deltaY - dragState.layoutTopDelta}px) scale(1.02)`;
  }

  function onDragPointerEnd(event) {
    if (!dragState || event.pointerId !== dragState.pointerId) return;
    const { row, handle, pointerId, startIndex, tentativeIndex, queueIndexOffset, moveHandler, upHandler, cancelHandler } = dragState;

    try { handle.releasePointerCapture(pointerId); } catch (err) { /* already released */ }
    document.removeEventListener('pointermove', moveHandler);
    document.removeEventListener('pointerup', upHandler);
    document.removeEventListener('pointercancel', cancelHandler);

    row.classList.remove('dragging');
    row.style.transform = '';
    document.body.style.userSelect = '';
    queueDragActive = false;
    dragState = null;

    if (tentativeIndex !== startIndex) {
      moveQueueTrack(startIndex - queueIndexOffset, tentativeIndex - queueIndexOffset);
    }
  }

  function onDragPointerDown(event, row, handle) {
    if (dragState || event.button !== 0) return;
    event.preventDefault();
    // setPointerCapture keeps the initial press pinned to `handle` even if
    // the cursor slips off it before the first move. It doesn't survive the
    // rest of the drag, though: reorderDom moves `row` (and thus `handle`)
    // to a new spot in the list via insertBefore/appendChild, and that DOM
    // reparenting silently drops capture. Once dropped, move/up would only
    // reach `handle` while the cursor happens to still be exactly over its
    // 18px width -- reliable when dragging slowly, but a fast drag easily
    // outruns the row's per-frame transform compensation and slips off,
    // silently killing the rest of the drag (dragState never clears,
    // queueDragActive stays stuck true, freezing all further queue
    // reconciliation). Listening on `document` instead means move/up keep
    // arriving regardless of where the cursor actually is or whether
    // capture survived.
    handle.setPointerCapture(event.pointerId);

    const rows = getQueueRows();
    const startIndex = rows.indexOf(row);
    if (startIndex === -1) return;
    // startIndex/tentativeIndex live in DOM-row space, which includes the
    // pinned now-playing row when present; the server's queue indices don't
    // include it, so this offset is subtracted before calling moveQueueTrack.
    const queueIndexOffset = rows[0] && rows[0].classList.contains('now-playing-row') ? 1 : 0;

    dragState = {
      pointerId: event.pointerId,
      row,
      handle,
      startIndex,
      tentativeIndex: startIndex,
      queueIndexOffset,
      startRect: row.getBoundingClientRect(),
      startClientY: event.clientY,
      layoutTopDelta: 0,
      moveHandler: null,
      upHandler: null,
      cancelHandler: null,
    };
    dragState.moveHandler = (e) => onDragPointerMove(e);
    dragState.upHandler = (e) => onDragPointerEnd(e);
    dragState.cancelHandler = (e) => onDragPointerEnd(e);

    row.classList.add('dragging');
    document.body.style.userSelect = 'none';
    queueDragActive = true;

    document.addEventListener('pointermove', dragState.moveHandler);
    document.addEventListener('pointerup', dragState.upHandler);
    document.addEventListener('pointercancel', dragState.cancelHandler);
  }

  function makeDragHandle(row) {
    const handle = document.createElement('div');
    handle.className = 'drag-handle flex items-center justify-center w-4.5 h-8.5 shrink-0 text-secondary cursor-grab touch-none';
    handle.title = 'Drag to reorder';
    handle.innerHTML = GRIP_ICON;
    handle.addEventListener('pointerdown', (event) => onDragPointerDown(event, row, handle));
    return handle;
  }

  function makeNowPlayingIndicator() {
    const el = document.createElement('div');
    el.className = 'now-playing-indicator flex items-end justify-center gap-0.5 w-4.5 h-8.5 shrink-0';
    el.innerHTML = '<span></span><span></span><span></span>';
    return el;
  }

  // The queue's remove button acts on the row's position, which can change
  // across renders even when the row itself persists (e.g. a track at
  // position 2 moves to position 1). The row's current index is stashed on
  // `dataset.index` by updateQueueRow on every reconcile, and the click
  // handler reads it at click-time instead of closing over a stale index
  // captured when the row was first created.
  function createQueueRow(track, index, items) {
    const row = createTrackRow(track);
    row.classList.add('queue-row');

    if (track.__nowPlaying) {
      row.insertBefore(makeNowPlayingIndicator(), row.firstChild);
      updateQueueRow(row, track, index, items);
      return row;
    }

    row.insertBefore(makeDragHandle(row), row.firstChild);

    const actions = document.createElement('div');
    actions.className = 'compact-actions shrink-0 flex gap-0.5';

    const playBtn = makeMiniIconBtn(PLAY_ICON, 'Play now', false, false);
    playBtn.addEventListener('click', () => playQueueTrack(Number(row.dataset.index)));
    actions.appendChild(playBtn);

    const removeBtn = makeMiniIconBtn(REMOVE_ICON, 'Remove', false, true);
    removeBtn.addEventListener('click', () => removeQueueTrack(Number(row.dataset.index)));
    actions.appendChild(removeBtn);

    row.appendChild(actions);
    updateQueueRow(row, track, index, items);
    return row;
  }

  // `items[0]` is the pinned now-playing entry (see renderQueue) when
  // present, so a real queue row's index is offset by one from its position
  // in the combined array.
  function updateQueueRow(row, track, index, items) {
    updateTrackRowContent(row, track);
    if (track.__nowPlaying) {
      row.classList.add('now-playing-row');
      row.classList.toggle('is-paused', !!track.__paused);
      return;
    }
    const offset = items && items[0] && items[0].__nowPlaying ? 1 : 0;
    const newIndex = String(index - offset);
    if (row.dataset.index !== newIndex) row.dataset.index = newIndex;
  }

  function renderQueue(snapshot) {
    const upcoming = snapshot.upcoming;
    document.getElementById('queue-count-badge').textContent = upcoming.length;
    document.getElementById('queue-shuffle-btn').disabled = upcoming.length < 2;
    document.getElementById('queue-clear-btn').disabled = upcoming.length === 0;
    document.getElementById('np-shuffle-btn').disabled = upcoming.length < 2;

    // A live snapshot (WebSocket push, ~every 1.5s, or an action response)
    // reflects server-confirmed state, which is stale mid-drag since the
    // move hasn't been sent yet — reconciling now would yank the dragged row
    // back out from under the cursor. Skip touching row DOM/order entirely
    // until the drag ends; normal reconciliation resumes on the next
    // snapshot after that.
    if (queueDragActive) return;

    const list = document.getElementById('queue-list');
    const emptyNote = document.getElementById('queue-empty-note');

    // Pin the currently playing/buffering track to the top of the queue
    // list, highlighted, instead of just letting it vanish from view now
    // that it's no longer in `upcoming`. Keyed separately from a plain
    // queue entry so a track playing twice back-to-back doesn't collide
    // with itself mid-reconcile.
    const nowPlaying = HERO_STATES.has(snapshot.state) && snapshot.track
      ? { ...snapshot.track, __nowPlaying: true, __paused: snapshot.state === 'paused' }
      : null;
    emptyNote.style.display = upcoming.length === 0 && !nowPlaying ? 'block' : 'none';

    const items = nowPlaying ? [nowPlaying, ...upcoming] : upcoming;
    reconcileList(
      list,
      items,
      (track) => (track.__nowPlaying ? `now-playing:${track.video_id}` : track.video_id),
      createQueueRow,
      updateQueueRow,
    );
  }

  // Fades `.np-info` out, swaps its text, then lets the same transition fade
  // it back in. Any pending swap from a previous rapid track change is
  // cleared first so overlapping calls don't cut each other's fade short.
  function crossfadeNowPlaying(applyContent) {
    const npInfo = document.getElementById('np-info');
    clearTimeout(npCrossfadeTimeout);
    npInfo.classList.add('np-fade-out');
    npCrossfadeTimeout = setTimeout(() => {
      applyContent();
      npInfo.classList.remove('np-fade-out');
      npCrossfadeTimeout = null;
    }, 150);
  }

  // Animates the progress fill with a single CSS transition to 100% timed
  // to the track's remaining length, rather than polling every tick — the
  // bar then advances smoothly between snapshots with no JS interval.
  function updateProgress(snapshot) {
    const wrap = document.getElementById('np-progress');
    const fill = document.getElementById('np-progress-fill');
    const elapsedEl = document.getElementById('np-elapsed');
    const duration = snapshot.track ? snapshot.track.duration_secs : null;
    const hasBar = !!snapshot.track && duration != null;
    const live = snapshot.state === 'playing' || snapshot.state === 'paused';
    if (hasBar && live && snapshot.position_ms == null) {
      // The bot has a track but couldn't say where it is (the audio worker
      // didn't answer in time): keep the bar up and hold it in place.
      freezeProgress();
      return;
    }
    const showProgress = hasBar && snapshot.position_ms != null;
    wrap.style.display = showProgress ? 'flex' : 'none';
    if (!showProgress) {
      elapsedEl.textContent = '';
      return;
    }

    const positionSecs = Math.min(snapshot.position_ms / 1000, duration);
    elapsedEl.textContent = formatDuration(positionSecs);
    progressFrozen = false;
    fill.style.transition = 'none';
    fill.style.width = `${duration > 0 ? (positionSecs / duration) * 100 : 0}%`;
    if (snapshot.state === 'playing') {
      void fill.offsetWidth; // flush so the transition below starts from the width just set
      const remaining = Math.max(duration - positionSecs, 0);
      fill.style.transition = `width ${remaining}s linear`;
      fill.style.width = '100%';
    }
  }

  // One-shot glow pulse on the art tile when a new track starts.
  function pulseNowPlayingArt() {
    const npArt = document.getElementById('np-art');
    npArt.classList.remove('np-pulse');
    void npArt.offsetWidth; // restart the animation if one is already mid-flight
    npArt.classList.add('np-pulse');
    npArt.addEventListener('animationend', () => npArt.classList.remove('np-pulse'), { once: true });
  }

  // The tab icon: the header's sun, drawn to hold together on a 16px tab
  // strip — a solid disc and eight rays with a clear gap between them,
  // filling the icon rather than sitting on a tile. Lit while audio is
  // running and dimmed when it is not, so a background tab shows at a
  // glance whether the bot is playing.
  const TAB_ICON_LIT = { from: '#f6c76a', mid: '#e3a53f', to: '#d9784a' };
  const TAB_ICON_DIM = { from: '#b99a63', mid: '#977a44', to: '#7f5a3e' };

  function tabIconSvg(tone) {
    let rays = '';
    for (let i = 0; i < 8; i++) {
      rays += `<line x1='16' y1='6' x2='16' y2='2' transform='rotate(${i * 45} 16 16)'/>`;
    }
    // A stroke has no width of its own to hang a gradient on, so the sun's
    // runs in user space and covers disc and rays alike.
    return `<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><defs>` +
      `<linearGradient id='s' gradientUnits='userSpaceOnUse' x1='5' y1='3' x2='27' y2='29'>` +
        `<stop offset='0' stop-color='${tone.from}'/><stop offset='.48' stop-color='${tone.mid}'/>` +
        `<stop offset='1' stop-color='${tone.to}'/></linearGradient></defs>` +
      `<circle cx='16' cy='16' r='6.2' fill='url(#s)'/>` +
      `<g stroke='url(#s)' stroke-width='3.6' stroke-linecap='round'>${rays}</g>` +
      `</svg>`;
  }

  let tabIconTone = null;
  function setTabIcon(playing) {
    const tone = playing ? TAB_ICON_LIT : TAB_ICON_DIM;
    if (tone === tabIconTone) return;
    tabIconTone = tone;
    document.getElementById('favicon').href = `data:image/svg+xml,${encodeURIComponent(tabIconSvg(tone))}`;
  }

  function renderSnapshot(snapshot) {
    setSnapshotStale(false);
    clearTimeout(staleTimer);
    staleTimer = setTimeout(() => setSnapshotStale(true), SNAPSHOT_STALE_MS);
    const playingVideoId = snapshot.track ? snapshot.track.video_id : null;
    const trackChanged = playingVideoId !== lastSeenPlayingVideoId;
    if (snapshot.state === 'playing' && trackChanged) {
      loadFavourites(currentGuildId);
    }
    // Only crossfade on an actual track-to-track switch — not on initial
    // load or on stopping, where there's no "before" text worth fading out.
    const shouldCrossfade = trackChanged && lastSeenPlayingVideoId !== null && playingVideoId !== null;
    lastSeenPlayingVideoId = playingVideoId;

    function applyNowPlayingContent() {
      document.getElementById('np-eyebrow').textContent = STATE_LABELS[snapshot.state] || snapshot.state;
      const hasTrack = !!snapshot.track;
      document.getElementById('np-title').textContent = hasTrack ? snapshot.track.title : '';
      document.getElementById('np-channel').textContent = hasTrack ? snapshot.track.channel : '';
      document.getElementById('np-duration').textContent = hasTrack ? formatDuration(snapshot.track.duration_secs) : '';
      if (hasTrack) applyThumbnail(document.getElementById('np-art'), snapshot.track.video_id, snapshot.track.video_id);
      updateProgress(snapshot);
    }

    if (shouldCrossfade) {
      crossfadeNowPlaying(applyNowPlayingContent);
      pulseNowPlayingArt();
    } else {
      applyNowPlayingContent();
    }

    renderHeroOrEmpty(snapshot);
    renderQueue(snapshot);

    const transportDisabled = snapshot.state !== 'playing' && snapshot.state !== 'paused';
    // A restart clears in-memory playback state but the queue is reloaded
    // straight from the DB (queue_snapshot), so `upcoming` can be non-empty
    // while `state` is still 'empty' — that's a resumable queue, not
    // nothing to do, so keep Play clickable (see togglePauseOrResume).
    document.getElementById('toggle-btn').disabled = transportDisabled && snapshot.upcoming.length === 0;
    document.getElementById('skip-btn').disabled = transportDisabled;
    document.getElementById('stop-btn').disabled = transportDisabled;

    dashboard.dataset.state = snapshot.state;
    setTabIcon(snapshot.state === 'playing');
    document.getElementById('play-icon').classList.toggle('visible', snapshot.state !== 'playing');
    document.getElementById('pause-icon').classList.toggle('visible', snapshot.state === 'playing');

    const radioBtn = document.getElementById('radio-btn');
    radioBtn.classList.toggle('active', !!snapshot.radio_enabled);

    const volumeSlider = document.getElementById('volume-slider');
    if (document.activeElement !== volumeSlider) {
      volumeSlider.value = snapshot.volume;
      volumeSlider.style.setProperty('--vol', `${snapshot.volume}%`);
    }
    document.getElementById('volume-label').textContent = `${snapshot.volume}%`;
  }

  async function sendAction(path, options) {
    try {
      const response = await api(`/api/guilds/${currentGuildId}/${path}`, options);
      const body = await response.json();
      if (!response.ok) {
        showStatus(body.error || 'Something went wrong.', { isError: true });
        return;
      }
      showStatus('');
      renderSnapshot(body);
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  document.getElementById('toggle-btn').addEventListener('click', () => togglePauseOrResume());
  document.getElementById('skip-btn').addEventListener('click', () => sendAction('skip', { method: 'POST' }));
  document.getElementById('stop-btn').addEventListener('click', () => sendAction('stop', { method: 'POST' }));
  document.getElementById('np-shuffle-btn').addEventListener('click', () => sendAction('shuffle', { method: 'POST' }));
  document.getElementById('queue-shuffle-btn').addEventListener('click', () => sendAction('shuffle', { method: 'POST' }));
  document.getElementById('radio-btn').addEventListener('click', () => sendAction('toggle-radio', { method: 'POST' }));
  document.getElementById('queue-clear-btn').addEventListener('click', () => sendAction('queue/clear', { method: 'POST' }));

  function moveQueueTrack(from, to) {
    sendAction(`queue/${from}/move`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ to }),
    });
  }

  function removeQueueTrack(index) {
    sendAction(`queue/${index}/remove`, { method: 'POST' });
  }

  async function playQueueTrack(index) {
    try {
      const response = await api(`/api/guilds/${currentGuildId}/queue/${index}/play`, { method: 'POST' });
      const body = await response.json();
      if (!response.ok) {
        if (body.error === NOT_CONNECTED_ERROR) {
          openJoinModal(() => playQueueTrack(index));
          return;
        }
        showStatus(body.error || 'Something went wrong.', { isError: true });
        return;
      }
      showStatus('');
      renderSnapshot(body);
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  const volumeBtn = document.getElementById('volume-btn');
  const volumePopover = document.getElementById('volume-popover');

  function closeVolumePopover() {
    volumePopover.classList.remove('open');
    volumeBtn.classList.remove('active');
    volumeBtn.setAttribute('aria-expanded', 'false');
  }

  volumeBtn.addEventListener('click', (event) => {
    event.stopPropagation();
    const opening = !volumePopover.classList.contains('open');
    volumePopover.classList.toggle('open', opening);
    volumeBtn.classList.toggle('active', opening);
    volumeBtn.setAttribute('aria-expanded', String(opening));
  });
  document.addEventListener('click', (event) => {
    if (!volumePopover.classList.contains('open')) return;
    if (volumeBtn.contains(event.target) || volumePopover.contains(event.target)) return;
    closeVolumePopover();
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && volumePopover.classList.contains('open')) closeVolumePopover();
  });

  document.getElementById('volume-slider').addEventListener('input', (event) => {
    document.getElementById('volume-label').textContent = `${event.target.value}%`;
    event.target.style.setProperty('--vol', `${event.target.value}%`);
    clearTimeout(volumeDebounce);
    const level = Number(event.target.value);
    volumeDebounce = setTimeout(() => {
      sendAction('volume', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ level }),
      });
    }, 250);
  });

  // ---- Join-a-voice-channel modal ----
  // Surfaced reactively: a queue/playlist action fails with PlayerError::
  // NotConnected (see src/voice/player.rs) whenever the bot has no active
  // voice connection in this guild, since nothing upstream of that point
  // knows which channel to join. Rather than just showing the error, offer
  // a channel picker and — once the user picks one and the join succeeds —
  // replay whatever action triggered the modal.
  const NOT_CONNECTED_ERROR = 'not connected to a voice channel';
  const NOTHING_PLAYING_ERROR = 'nothing is playing';
  const VOICE_ICON = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H2v6h4l5 4V5z"/><path d="M15.54 8.46a5 5 0 0 1 0 7.07"/></svg>';

  const joinModalOverlay = document.getElementById('join-modal-overlay');
  const joinChannelList = document.getElementById('join-channel-list');
  const joinChannelEmptyNote = document.getElementById('join-channel-empty-note');
  const joinChannelLoadingNote = document.getElementById('join-channel-loading-note');
  let pendingRetryAction = null;

  function closeJoinModal() {
    joinModalOverlay.classList.remove('open');
    pendingRetryAction = null;
  }

  function createChannelRow(channel, isLastUsed) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'channel-row flex items-center gap-3 w-full h-11 px-2.5 rounded text-left text-base text-white hover:bg-hover';

    const icon = document.createElement('span');
    icon.className = 'channel-row-icon flex items-center justify-center shrink-0 text-secondary' + (isLastUsed ? ' last-used' : '');
    icon.innerHTML = VOICE_ICON;
    row.appendChild(icon);

    const name = document.createElement('span');
    name.className = 'flex-1 min-w-0 truncate';
    name.textContent = channel.name;
    row.appendChild(name);

    row.addEventListener('click', () => joinChannel(channel.id, channel.name));
    return row;
  }

  async function openJoinModal(retryAction) {
    pendingRetryAction = retryAction || null;
    joinModalOverlay.classList.add('open');
    joinChannelList.innerHTML = '';
    joinChannelEmptyNote.style.display = 'none';
    joinChannelLoadingNote.style.display = 'flex';
    try {
      const response = await api(`/api/guilds/${currentGuildId}/voice-channels`);
      if (!response.ok) return;
      const channels = await response.json();
      if (channels.length === 0) {
        joinChannelEmptyNote.style.display = 'block';
        return;
      }
      const lastChannelId = localStorage.getItem(lastChannelKey(currentGuildId));
      const lastIndex = channels.findIndex((c) => c.id === lastChannelId);
      if (lastIndex > 0) channels.unshift(channels.splice(lastIndex, 1)[0]);
      for (const channel of channels) {
        joinChannelList.appendChild(createChannelRow(channel, channel.id === lastChannelId));
      }
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      joinChannelLoadingNote.style.display = 'none';
    }
  }

  async function joinChannel(channelId, channelName) {
    try {
      const response = await api(`/api/guilds/${currentGuildId}/join`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ channel_id: channelId }),
      });
      const body = await response.json();
      if (!response.ok) {
        showStatus(body.error || 'Could not join that channel.', { isError: true });
        return;
      }
      localStorage.setItem(lastChannelKey(currentGuildId), channelId);
      renderSnapshot(body);
      showStatus(`Joined "${channelName}".`);
      const retry = pendingRetryAction;
      closeJoinModal();
      if (retry) retry();
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  // The toggle button doubles as "resume a persisted queue" after a
  // restart: the process reloads `upcoming` straight from the DB (see
  // queue_snapshot in src/voice/player.rs) so it's non-empty even though
  // nothing is playing yet, but pause()/resume() only ever look at the
  // in-memory current-track handle and fail with "nothing is playing"
  // until the bot rejoins a voice channel. Joining is what actually starts
  // the persisted queue (restore_session_if_new), so surface the same
  // channel picker used for queueing/playlists instead of leaving the
  // button inert.
  async function togglePauseOrResume() {
    try {
      const response = await api(`/api/guilds/${currentGuildId}/toggle-pause`, { method: 'POST' });
      const body = await response.json();
      if (!response.ok) {
        const upcomingCount = Number(document.getElementById('queue-count-badge').textContent) || 0;
        if (body.error === NOTHING_PLAYING_ERROR && upcomingCount > 0) {
          openJoinModal(null);
          return;
        }
        showStatus(body.error || 'Something went wrong.', { isError: true });
        return;
      }
      showStatus('');
      renderSnapshot(body);
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  document.getElementById('join-modal-cancel').addEventListener('click', closeJoinModal);
  joinModalOverlay.addEventListener('click', (event) => {
    if (event.target === joinModalOverlay) closeJoinModal();
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && joinModalOverlay.classList.contains('open')) closeJoinModal();
  });

  // ---- Search ----
  // Faithful mirror of the backend's extract_video_id (src/commands/playback.rs)
  // so "is this a link" is decided identically client-side and server-side.
  function extractVideoId(input) {
    let url;
    try { url = new URL(input); } catch { return null; }
    const host = url.hostname;
    if (host === 'youtu.be') {
      const seg = url.pathname.split('/').filter(Boolean)[0];
      return seg || null;
    }
    if (host === 'youtube.com' || host.endsWith('.youtube.com')) {
      if (url.pathname === '/watch') {
        return url.searchParams.get('v');
      }
      if (url.pathname.startsWith('/shorts/')) {
        const rest = url.pathname.slice('/shorts/'.length);
        const id = rest.split('/')[0];
        return id || null;
      }
      return null;
    }
    return null;
  }

  // A YouTube link carrying a `list` parameter (and no video id, which
  // extractVideoId handles first) is previewed as a playlist rather than
  // searched for; the server re-validates the host before listing it.
  function looksLikePlaylistUrl(input) {
    let url;
    try { url = new URL(input); } catch { return false; }
    const host = url.hostname;
    const recognizedHost = host === 'youtu.be' || host === 'youtube.com' || host.endsWith('.youtube.com');
    return recognizedHost && url.searchParams.has('list');
  }

  const topbarSearch = document.getElementById('topbar-search');
  const searchDropdown = document.getElementById('search-dropdown');
  const searchInput = document.getElementById('search-input');
  const searchResults = document.getElementById('search-results');
  const searchEmptyNote = document.getElementById('search-empty-note');
  const searchLoadingNote = document.getElementById('search-loading-note');
  const searchLoadingText = document.getElementById('search-loading-text');

  function openSearchDropdown() {
    searchDropdown.classList.add('open');
  }

  function closeSearchDropdown() {
    searchDropdown.classList.remove('open');
  }

  function clearSearch() {
    searchInput.value = '';
    searchResults.innerHTML = '';
    searchEmptyNote.style.display = 'none';
    searchLoadingNote.style.display = 'none';
    searchActiveIndex = -1;
    closeSearchDropdown();
  }

  // Every search row carries `_activate`, the action its Add button and the
  // Enter key share: a track row adds that track, a playlist row imports the
  // playlist and starts it.
  function appendSearchAddButton(row) {
    const addBtn = document.createElement('button');
    addBtn.type = 'button';
    addBtn.className = 'outline-btn';
    addBtn.textContent = 'Add';
    addBtn.addEventListener('click', () => row._activate(addBtn));
    row.appendChild(addBtn);
  }

  function createSearchRow(track) {
    const row = createTrackRow(track);
    appendSearchAddButton(row);
    updateSearchRow(row, track);
    return row;
  }

  function updateSearchRow(row, track) {
    updateTrackRowContent(row, track);
    row._activate = () => {
      addToQueue(track.video_id, track.title);
      closeSearchDropdown();
    };
  }

  function playlistCountLabel(trackCount) {
    if (trackCount == null) return 'Playlist';
    return `Playlist · ${trackCount} track${trackCount === 1 ? '' : 's'}`;
  }

  function createPlaylistSearchRow(preview) {
    const row = document.createElement('div');
    row.className = 'compact-row relative flex items-center gap-3 min-h-14 p-2 rounded shrink-0 overflow-hidden hover:bg-hover';

    const thumb = document.createElement('div');
    thumb.className = 'compact-thumb thumb relative size-10 rounded shrink-0 overflow-hidden';
    applyThumbnail(thumb, preview.thumbnail_video_id, preview.url);
    row.appendChild(thumb);

    const meta = document.createElement('div');
    meta.className = 'compact-meta min-w-0 flex-1 flex flex-col gap-0.5';
    const titleSpan = document.createElement('span');
    titleSpan.className = 'compact-title text-base truncate';
    titleSpan.textContent = preview.name;
    meta.appendChild(titleSpan);
    const sub = document.createElement('div');
    sub.className = 'compact-channel text-sm text-secondary truncate';
    sub.textContent = playlistCountLabel(preview.track_count);
    meta.appendChild(sub);
    row.appendChild(meta);

    appendSearchAddButton(row);
    row._activate = (btn) => importAndPlayPlaylist(preview.url, btn);
    return row;
  }

  function renderSearchResults(tracks) {
    searchEmptyNote.style.display = tracks.length === 0 ? 'block' : 'none';
    reconcileList(searchResults, tracks, (track) => track.video_id, createSearchRow, updateSearchRow);
    openSearchDropdown();
    setSearchActiveIndex(-1);
  }

  function renderPlaylistPreview(preview) {
    searchEmptyNote.style.display = 'none';
    searchResults.innerHTML = '';
    const row = createPlaylistSearchRow(preview);
    searchResults.appendChild(row);
    animateEnter(row);
    openSearchDropdown();
    setSearchActiveIndex(-1);
  }

  function setSearchActiveIndex(index) {
    const rows = Array.from(searchResults.children);
    if (rows.length === 0) index = -1;
    else if (index >= rows.length) index = -1;
    else if (index < -1) index = rows.length - 1;
    rows.forEach((row, i) => row.classList.toggle('kbd-active', i === index));
    if (index !== -1) rows[index].scrollIntoView({ block: 'nearest' });
    searchActiveIndex = index;
  }

  function setSearchBarLoading(loading) {
    document.getElementById('search-bar').classList.toggle('loading', loading);
  }

  function showSearchLoading(text) {
    searchLoadingText.textContent = text;
    searchLoadingNote.style.display = 'flex';
    searchEmptyNote.style.display = 'none';
    openSearchDropdown();
  }

  // Runs one search-bar lookup, guarded by `searchSeq` so a slow response
  // never lands over a newer query. `path` is the API call, `onResult` is
  // handed the parsed body of a successful response.
  async function runSearchLookup(path, failureMessage, onResult) {
    const seq = ++searchSeq;
    setSearchBarLoading(true);
    try {
      const response = await api(path);
      const body = await response.json();
      if (seq !== searchSeq) return; // a newer search superseded this one
      searchLoadingNote.style.display = 'none';
      if (!response.ok) {
        showStatus(body.error || failureMessage, { isError: true });
        searchResults.innerHTML = '';
        searchEmptyNote.style.display = 'none';
        closeSearchDropdown();
        return;
      }
      showStatus('');
      onResult(body);
    } catch (err) {
      if (seq === searchSeq) searchLoadingNote.style.display = 'none';
    } finally {
      if (seq === searchSeq) setSearchBarLoading(false);
    }
  }

  function runSearch(query, isLink) {
    if (!isLink) showSearchLoading('Searching…');
    const path = `/api/guilds/${currentGuildId}/search?q=${encodeURIComponent(query)}`;
    return runSearchLookup(path, 'Search failed.', (body) => {
      if (isLink && Array.isArray(body) && body.length > 0) {
        // Nothing to disambiguate for a recognized link — add it straight away.
        searchInput.value = '';
        searchResults.innerHTML = '';
        searchEmptyNote.style.display = 'none';
        closeSearchDropdown();
        addToQueue(body[0].video_id, body[0].title);
        return;
      }
      renderSearchResults(body);
    });
  }

  function runPlaylistPreview(url) {
    showSearchLoading('Loading playlist…');
    const path = `/api/guilds/${currentGuildId}/playlists/preview?url=${encodeURIComponent(url)}`;
    return runSearchLookup(path, 'Could not load that playlist.', renderPlaylistPreview);
  }

  searchInput.addEventListener('input', () => {
    clearTimeout(searchDebounce);
    const query = searchInput.value.trim();
    if (!query) {
      searchResults.innerHTML = '';
      searchEmptyNote.style.display = 'none';
      searchLoadingNote.style.display = 'none';
      closeSearchDropdown();
      searchActiveIndex = -1;
      return;
    }
    const isLink = extractVideoId(query) !== null;
    const isPlaylistLink = !isLink && looksLikePlaylistUrl(query);
    if (isLink || isPlaylistLink) {
      // A link resolves to something unrelated to any earlier results — clear
      // the stale list immediately instead of leaving it visible mid-flight.
      searchResults.innerHTML = '';
      searchEmptyNote.style.display = 'none';
      closeSearchDropdown();
      searchActiveIndex = -1;
    }
    if (isPlaylistLink) {
      searchDebounce = setTimeout(() => runPlaylistPreview(query), 300);
      return;
    }
    searchDebounce = setTimeout(() => runSearch(query, isLink), 300);
  });

  searchInput.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') { closeSearchDropdown(); return; }
    if (!searchDropdown.classList.contains('open') || searchResults.children.length === 0) return;
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      setSearchActiveIndex(searchActiveIndex + 1);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      setSearchActiveIndex(searchActiveIndex - 1);
    } else if (event.key === 'Enter') {
      const row = searchResults.children[searchActiveIndex === -1 ? 0 : searchActiveIndex];
      if (row && row._activate) {
        event.preventDefault();
        row._activate();
      }
    }
  });

  async function addToQueue(videoId, title, btn) {
    setButtonBusy(btn, true);
    try {
      const response = await api(`/api/guilds/${currentGuildId}/queue/add`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ video_id: videoId }),
      });
      const body = await response.json();
      if (!response.ok) {
        if (body.error === NOT_CONNECTED_ERROR) {
          openJoinModal(() => addToQueue(videoId, title));
          return;
        }
        showStatus(body.error || 'Could not add that track.', { isError: true });
        return;
      }
      renderSnapshot(body);
      showStatus(`Added "${title}" to the queue.`);
      loadFavourites(currentGuildId);
      clearSearch();
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      setButtonBusy(btn, false);
    }
  }

  // Picking a playlist from the search dropdown saves it to the guild's
  // playlists and queues it in one go, so the row behaves like a track's Add.
  async function importAndPlayPlaylist(url, btn) {
    setButtonBusy(btn, true);
    setSearchBarLoading(true);
    let imported = null;
    try {
      const response = await api(`/api/guilds/${currentGuildId}/playlists/import`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ url }),
      });
      const body = await response.json();
      if (!response.ok) {
        showStatus(body.error || 'Could not import that playlist.', { isError: true });
        return;
      }
      imported = body;
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      setSearchBarLoading(false);
      setButtonBusy(btn, false);
    }
    if (!imported) return;
    clearSearch();
    loadPlaylists(currentGuildId);
    await playPlaylist(imported.id, imported.name);
  }

  async function playTrackNow(videoId, title, btn) {
    setButtonBusy(btn, true);
    try {
      const response = await api(`/api/guilds/${currentGuildId}/play`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ video_id: videoId }),
      });
      const body = await response.json();
      if (!response.ok) {
        if (body.error === NOT_CONNECTED_ERROR) {
          openJoinModal(() => playTrackNow(videoId, title));
          return;
        }
        showStatus(body.error || 'Could not play that track.', { isError: true });
        return;
      }
      renderSnapshot(body);
      showStatus(`Playing "${title}".`);
      loadFavourites(currentGuildId);
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      setButtonBusy(btn, false);
    }
  }

  // ---- Playlists ----
  const playlistRow = document.getElementById('playlist-row');

  // A card's cover is its play control: clicking the artwork starts the
  // playlist, so the card itself carries no Play button.
  function createPlaylistCover() {
    const cover = document.createElement('button');
    cover.type = 'button';
    cover.title = 'Play playlist';
    cover.className = 'playlist-cover thumb relative flex items-center justify-center w-full aspect-square rounded-lg mb-2 overflow-hidden';
    return cover;
  }

  function applyPlaylistCover(cover, playlist) {
    applyThumbnail(cover, playlist.thumbnail_video_id, String(playlist.id));
    if (cover.classList.contains('playlist-cover')) cover.appendChild(makeCoverPlayGlyph());
  }

  function renderPlaylists(playlists) {
    playlistRow.innerHTML = '';
    for (const playlist of playlists) {
      const chip = document.createElement('div');
      chip.className = 'playlist-chip relative flex flex-col gap-0.5 min-w-0';

      const cover = createPlaylistCover();
      applyPlaylistCover(cover, playlist);
      cover.addEventListener('click', () => playPlaylist(playlist.id, playlist.name, cover));
      chip.appendChild(cover);

      const name = document.createElement('div');
      name.className = 'playlist-name text-base font-medium truncate';
      name.textContent = playlist.name;
      chip.appendChild(name);

      const count = document.createElement('div');
      count.className = 'playlist-count text-sm text-secondary truncate';
      count.textContent = `${playlist.track_count} track${playlist.track_count === 1 ? '' : 's'}`;
      chip.appendChild(count);

      const actions = document.createElement('div');
      actions.className = 'playlist-actions flex items-center justify-end gap-1';

      const refreshBtn = makeMiniIconBtn(REFRESH_ICON, 'Refresh from YouTube', false, false);
      refreshBtn.addEventListener('click', () => refreshPlaylist(playlist.id, playlist.name));
      actions.appendChild(refreshBtn);

      const removeBtn = makeMiniIconBtn(REMOVE_ICON, 'Remove playlist', false, true);
      removeBtn.addEventListener('click', () => removePlaylist(playlist.id, playlist.name));
      actions.appendChild(removeBtn);

      chip.appendChild(actions);

      playlistRow.appendChild(chip);
    }
    renderSidebarPlaylists(playlists);
  }

  // The sidebar lists the same playlists as compact rows: a click plays one,
  // while refresh/remove stay on the cards in the main view.
  const sidebarPlaylists = document.getElementById('sidebar-playlists');

  function renderSidebarPlaylists(playlists) {
    sidebarPlaylists.innerHTML = '';
    for (const playlist of playlists) {
      const item = document.createElement('button');
      item.type = 'button';
      item.className = 'sidebar-item flex items-center gap-3 w-full h-12 px-2 rounded-lg text-white text-left shrink-0 hover:bg-hover';
      item.title = playlist.name;

      const cover = document.createElement('span');
      cover.className = 'thumb relative size-8 rounded shrink-0 overflow-hidden';
      applyPlaylistCover(cover, playlist);
      item.appendChild(cover);

      const text = document.createElement('span');
      text.className = 'sidebar-item-text min-w-0 flex-1 flex flex-col gap-0.5';
      const title = document.createElement('span');
      title.className = 'text-base font-medium truncate';
      title.textContent = playlist.name;
      text.appendChild(title);
      const sub = document.createElement('span');
      sub.className = 'text-xs text-secondary truncate';
      sub.textContent = `Playlist · ${playlist.track_count} track${playlist.track_count === 1 ? '' : 's'}`;
      text.appendChild(sub);
      item.appendChild(text);

      item.addEventListener('click', () => playPlaylist(playlist.id, playlist.name, item));
      sidebarPlaylists.appendChild(item);
    }
  }

  async function loadPlaylists(guildId) {
    try {
      const response = await api(`/api/guilds/${guildId}/playlists`);
      if (!response.ok) {
        renderPlaylists([]);
        return;
      }
      renderPlaylists(await response.json());
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  async function playPlaylist(id, name, btn) {
    setButtonBusy(btn, true);
    try {
      const response = await api(`/api/guilds/${currentGuildId}/playlists/${id}/play`, { method: 'POST' });
      const body = await response.json();
      if (!response.ok) {
        if (body.error === NOT_CONNECTED_ERROR) {
          openJoinModal(() => playPlaylist(id, name));
          return;
        }
        showStatus(body.error || 'Could not start that playlist.', { isError: true });
        return;
      }
      renderSnapshot(body);
      showStatus(`Playing "${name}".`);
      loadFavourites(currentGuildId);
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      setButtonBusy(btn, false);
    }
  }

  async function refreshPlaylist(id, name) {
    try {
      const response = await api(`/api/guilds/${currentGuildId}/playlists/${id}/refresh`, { method: 'POST' });
      const body = await response.json();
      if (!response.ok) {
        showStatus(body.error || 'Could not refresh that playlist.', { isError: true });
        return;
      }
      showStatus(`Refreshed "${name}" (${body.track_count} track${body.track_count === 1 ? '' : 's'}).`);
      loadPlaylists(currentGuildId);
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  async function removePlaylist(id, name) {
    if (!window.confirm(`Remove "${name}"? You can re-import it later from its URL.`)) return;
    try {
      const response = await api(`/api/guilds/${currentGuildId}/playlists/${id}/remove`, { method: 'POST' });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        showStatus(body.error || 'Could not remove that playlist.', { isError: true });
        return;
      }
      showStatus(`Removed "${name}".`);
      loadPlaylists(currentGuildId);
      loadFavourites(currentGuildId);
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  // ---- Favourites ----
  const favouriteTracksList = document.getElementById('favourite-tracks');
  const favouriteTracksEmptyNote = document.getElementById('favourite-tracks-empty-note');
  const favouritePlaylistsRow = document.getElementById('favourite-playlists');
  const favouritePlaylistsEmptyNote = document.getElementById('favourite-playlists-empty-note');

  // The play count rides in the row's sub-line next to the channel rather
  // than in the duration slot: these cards are narrow, and a combined
  // "4:14 · played 40×" there takes the width from the title, which matters
  // more than either.
  function applyFavouritePlayCount(row, track) {
    const channelEl = row.querySelector('.compact-channel');
    const playCountText = `played ${track.play_count}×`;
    channelEl.textContent = track.channel
      ? `${track.channel} · ${playCountText}`
      : playCountText;
  }

  // The row itself plays the track right away (the thumbnail shows a play
  // glyph on hover to say so); the Add button queues it instead, and stops
  // its click short of the row so it does not also play.
  function createFavouriteTrackRow(track) {
    const row = createTrackRow(track);
    row.classList.add('play-row', 'cursor-pointer');
    row.title = 'Play now';
    row.tabIndex = 0;
    row.setAttribute('role', 'button');
    const thumb = row.querySelector('.compact-thumb');
    thumb.appendChild(makeCoverPlayGlyph());
    const play = () => playTrackNow(row._videoId, row._title, thumb);
    row.addEventListener('click', play);
    row.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); play(); }
    });

    const addBtn = document.createElement('button');
    addBtn.type = 'button';
    addBtn.className = 'outline-btn';
    addBtn.textContent = 'Add';
    addBtn.title = 'Add to queue';
    addBtn.addEventListener('click', (event) => {
      event.stopPropagation();
      addToQueue(row._videoId, row._title, addBtn);
    });
    row.appendChild(addBtn);
    updateFavouriteTrackRow(row, track);
    return row;
  }

  function updateFavouriteTrackRow(row, track) {
    updateTrackRowContent(row, track);
    applyFavouritePlayCount(row, track);
    row._videoId = track.video_id;
    row._title = track.title;
  }

  function renderFavouriteTracks(tracks) {
    favouriteTracksEmptyNote.style.display = tracks.length === 0 ? 'block' : 'none';
    reconcileList(favouriteTracksList, tracks, (track) => track.video_id, createFavouriteTrackRow, updateFavouriteTrackRow);
  }

  function createFavPlaylistChip(playlist) {
    const chip = document.createElement('div');
    chip.className = 'playlist-chip relative flex flex-col gap-0.5 min-w-0';

    const cover = createPlaylistCover();
    cover.addEventListener('click', () => playPlaylist(chip._playlistId, chip._playlistName, cover));
    chip.appendChild(cover);

    const name = document.createElement('div');
    name.className = 'playlist-name text-base font-medium truncate';
    chip.appendChild(name);

    const count = document.createElement('div');
    count.className = 'playlist-count text-sm text-secondary truncate';
    chip.appendChild(count);

    updateFavPlaylistChip(chip, playlist);
    return chip;
  }

  function updateFavPlaylistChip(chip, playlist) {
    applyPlaylistCover(chip.querySelector('.playlist-cover'), playlist);
    chip.querySelector('.playlist-name').textContent = playlist.name;
    chip.querySelector('.playlist-count').textContent =
      `Played ${playlist.play_count} time${playlist.play_count === 1 ? '' : 's'}`;
    chip._playlistId = playlist.id;
    chip._playlistName = playlist.name;
  }

  function renderFavouritePlaylists(playlists) {
    favouritePlaylistsEmptyNote.style.display = playlists.length === 0 ? 'block' : 'none';
    reconcileList(favouritePlaylistsRow, playlists, (playlist) => String(playlist.id), createFavPlaylistChip, updateFavPlaylistChip);
  }

  async function loadFavourites(guildId) {
    try {
      const response = await api(`/api/guilds/${guildId}/favourites`);
      if (!response.ok) {
        renderFavouriteTracks([]);
        renderFavouritePlaylists([]);
        return;
      }
      const body = await response.json();
      renderFavouriteTracks(body.tracks);
      renderFavouritePlaylists(body.playlists);
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  // ---- Users (admin only) ----
  const navDashboardBtn = document.getElementById('nav-dashboard-btn');
  const navUsersBtn = document.getElementById('nav-users-btn');
  const pageDashboardBody = document.getElementById('page-dashboard-body');
  const pageUsers = document.getElementById('page-users');
  const usersList = document.getElementById('users-list');
  const usersEmptyNote = document.getElementById('users-empty-note');
  let currentUser = null;

  // The nav itself is hidden from non-admins (see loadCurrentUser), but
  // showPage stays reachable either way so showLogin can always reset back
  // to the Dashboard page.
  function showPage(name) {
    const onUsers = name === 'users';
    pageDashboardBody.style.display = onUsers ? 'none' : 'flex';
    pageUsers.style.display = onUsers ? 'flex' : 'none';
    topbarSearch.style.display = onUsers ? 'none' : '';
    dashboard.classList.toggle('on-users', onUsers);
    navDashboardBtn.classList.toggle('active', !onUsers);
    navUsersBtn.classList.toggle('active', onUsers);
    closeDrawers();
    if (onUsers) loadUsers();
  }

  // ---- Sidebar and queue drawers ----
  // Above 1200px all three columns fit. Between 900 and 1199 the sidebar
  // starts collapsed to icons and the queue slides in over the content;
  // below 900 the sidebar is a drawer and the queue a sheet. Both close on
  // Escape, on the backdrop, or on any page switch.
  const narrowViewport = window.matchMedia('(max-width: 899px)');
  const midViewport = window.matchMedia('(max-width: 1199px)');
  const queueToggleBtn = document.getElementById('queue-toggle-btn');
  dashboard.classList.toggle('sidebar-mini', midViewport.matches && !narrowViewport.matches);

  function closeDrawers() {
    dashboard.classList.remove('sidebar-open', 'queue-open');
    queueToggleBtn.classList.remove('active');
  }

  document.getElementById('sidebar-toggle-btn').addEventListener('click', () => {
    if (narrowViewport.matches) {
      dashboard.classList.remove('queue-open');
      queueToggleBtn.classList.remove('active');
      dashboard.classList.toggle('sidebar-open');
    } else {
      dashboard.classList.toggle('sidebar-mini');
    }
  });
  document.getElementById('sidebar-close-btn').addEventListener('click', closeDrawers);
  queueToggleBtn.addEventListener('click', () => {
    dashboard.classList.remove('sidebar-open');
    const opening = !dashboard.classList.contains('queue-open');
    dashboard.classList.toggle('queue-open', opening);
    queueToggleBtn.classList.toggle('active', opening);
  });
  document.getElementById('queue-close-btn').addEventListener('click', closeDrawers);
  document.getElementById('scrim').addEventListener('click', closeDrawers);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') closeDrawers();
  });

  // Importing happens by pasting a playlist link into search, so the sidebar
  // button points there.
  const DEFAULT_SEARCH_PLACEHOLDER = searchInput.placeholder;
  document.getElementById('new-playlist-btn').addEventListener('click', () => {
    showPage('dashboard');
    searchInput.placeholder = 'Paste a YouTube playlist link to save it';
    searchInput.focus();
    showStatus('Paste a YouTube playlist link into the search bar to save it as a playlist.');
  });
  searchInput.addEventListener('blur', () => {
    searchInput.placeholder = DEFAULT_SEARCH_PLACEHOLDER;
  });

  navDashboardBtn.addEventListener('click', () => showPage('dashboard'));
  navUsersBtn.addEventListener('click', () => showPage('users'));

  async function loadCurrentUser() {
    try {
      const response = await api('/api/me');
      if (!response.ok) return;
      currentUser = await response.json();
      navUsersBtn.style.display = currentUser.is_admin ? '' : 'none';
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  function createUserRow(user) {
    const row = document.createElement('div');
    row.className = 'compact-row relative flex items-center gap-3 min-h-14 p-2 rounded shrink-0 overflow-hidden hover:bg-hover';

    const meta = document.createElement('div');
    meta.className = 'compact-meta min-w-0 flex-1 flex flex-col gap-0.5';
    const title = document.createElement('div');
    title.className = 'compact-title text-base truncate';
    meta.appendChild(title);
    const scope = document.createElement('div');
    scope.className = 'user-scope text-sm text-secondary truncate';
    meta.appendChild(scope);
    row.appendChild(meta);

    const badge = document.createElement('div');
    badge.className = 'admin-badge shrink-0 text-2xs font-medium text-white bg-hover px-2.5 py-0.5 rounded-full';
    row.appendChild(badge);

    const guildBtn = makeMiniIconBtn(SERVER_ICON, 'Server access', false, false);
    guildBtn.classList.add('user-guild-btn');
    guildBtn.addEventListener('click', () => openAssignGuildModal(row._username, row._guildId));
    row.appendChild(guildBtn);

    const renameBtn = makeMiniIconBtn(USER_ICON, 'Rename user', false, false);
    renameBtn.classList.add('rename-user-btn');
    renameBtn.addEventListener('click', () => openRenameUserModal(row._username));
    row.appendChild(renameBtn);

    const editBtn = makeMiniIconBtn(EDIT_ICON, 'Change password', false, false);
    editBtn.classList.add('edit-user-btn');
    editBtn.addEventListener('click', () => openEditPasswordModal(row._username));
    row.appendChild(editBtn);

    const removeBtn = makeMiniIconBtn(REMOVE_ICON, 'Delete user', false, true);
    removeBtn.addEventListener('click', () => deleteUser(row._username));
    row.appendChild(removeBtn);

    updateUserRow(row, user);
    return row;
  }

  // The admin badge uses visibility (not display) when hidden so every row
  // keeps the same set of slots — otherwise a plain-user row would lose one
  // and its buttons would drift out of alignment.
  function updateUserRow(row, user) {
    row.querySelector('.compact-title').textContent = user.username;
    const badge = row.querySelector('.admin-badge');
    badge.textContent = user.is_root ? 'Root' : 'Admin';
    badge.style.visibility = user.is_admin ? 'visible' : 'hidden';

    const isSelf = !!currentUser && user.username === currentUser.username;

    // Everyone can rename/reset their own account; only root can do either
    // to someone else — see the matching checks in web::users::set_username
    // and web::users::set_password.
    const canEditSelfOrRoot = isSelf || (!!currentUser && currentUser.is_root);

    const renameBtn = row.querySelector('.rename-user-btn');
    renameBtn.disabled = !canEditSelfOrRoot;
    renameBtn.title = canEditSelfOrRoot
      ? 'Rename user'
      : "Only the root admin can rename another user";

    const editBtn = row.querySelector('.edit-user-btn');
    editBtn.disabled = !canEditSelfOrRoot;
    editBtn.title = canEditSelfOrRoot
      ? 'Change password'
      : "Only the root admin can change another user's password";

    const removeBtn = row.querySelector('.mini-icon-btn.danger');
    removeBtn.disabled = isSelf || user.is_root;
    removeBtn.title = user.is_root
      ? "The root account can't be deleted"
      : isSelf
        ? "You can't delete your own account"
        : 'Delete user';

    // Admins reach every server regardless of what's stored against them, so
    // the row says so rather than showing a pin that isn't being enforced —
    // see web::auth::CurrentUser::may_access_guild.
    row.querySelector('.user-scope').textContent = user.is_admin
      ? 'All servers'
      : guildLabel(user.guild_id);

    row._username = user.username;
    row._guildId = user.guild_id || null;
  }

  // The server a user is pinned to, by name where the bot still knows it —
  // an id the cache has no guild for (the bot was removed from it, say)
  // falls back to the raw id so the row still says something true.
  function guildLabel(guildId) {
    if (!guildId) return 'All servers';
    const guild = guilds.find((g) => g.id === guildId);
    return guild ? guild.name : `Server ${guildId}`;
  }

  // "All servers" plus every guild this dashboard can see. An admin editing
  // someone pinned to a guild the admin themselves can't see would drop that
  // pin silently, so the current value is always included as an option.
  function fillGuildSelect(select, selectedId) {
    select.innerHTML = '';
    const options = [{ id: '', name: 'All servers' }, ...guilds];
    if (selectedId && !guilds.some((g) => g.id === selectedId)) {
      options.push({ id: selectedId, name: guildLabel(selectedId) });
    }
    for (const option of options) {
      const el = document.createElement('option');
      el.value = option.id;
      el.textContent = option.name;
      select.appendChild(el);
    }
    select.value = selectedId || '';
  }

  async function loadUsers() {
    try {
      const response = await api('/api/users');
      if (!response.ok) return;
      const users = await response.json();
      usersEmptyNote.style.display = users.length === 0 ? 'block' : 'none';
      reconcileList(usersList, users, (user) => user.username, createUserRow, updateUserRow);
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  async function deleteUser(username) {
    if (!window.confirm(`Remove "${username}"? They will no longer be able to sign in.`)) return;
    try {
      const response = await api(`/api/users/${encodeURIComponent(username)}`, { method: 'DELETE' });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        showStatus(body.error || 'Could not remove that user.', { isError: true });
        return;
      }
      showStatus(`Removed "${username}".`);
      loadUsers();
    } catch (err) { /* showLogin already handled unauthorized */ }
  }

  // ---- Add-user modal ----
  const addUserModalOverlay = document.getElementById('add-user-modal-overlay');
  const addUserUsernameInput = document.getElementById('new-user-username');
  const addUserPasswordInput = document.getElementById('new-user-password');
  const addUserIsAdminInput = document.getElementById('new-user-is-admin');
  const addUserGuildSelect = document.getElementById('new-user-guild');
  const addUserError = document.getElementById('add-user-error');
  const addUserSubmitBtn = document.getElementById('add-user-submit');

  function openAddUserModal() {
    addUserUsernameInput.value = '';
    addUserPasswordInput.value = '';
    addUserIsAdminInput.checked = false;
    fillGuildSelect(addUserGuildSelect, '');
    addUserError.textContent = '';
    addUserModalOverlay.classList.add('open');
    addUserUsernameInput.focus();
  }

  function closeAddUserModal() {
    addUserModalOverlay.classList.remove('open');
  }

  async function submitAddUser() {
    const username = addUserUsernameInput.value.trim();
    const password = addUserPasswordInput.value;
    if (!username || !password) {
      addUserError.textContent = 'Username and password are required.';
      return;
    }
    addUserError.textContent = '';
    setButtonBusy(addUserSubmitBtn, true);
    try {
      const response = await api('/api/users', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          username,
          password,
          is_admin: addUserIsAdminInput.checked,
          guild_id: addUserGuildSelect.value || null,
        }),
      });
      const body = await response.json().catch(() => ({}));
      if (!response.ok) {
        addUserError.textContent = body.error || 'Could not create that user.';
        return;
      }
      closeAddUserModal();
      showStatus(`Created "${username}".`);
      loadUsers();
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      setButtonBusy(addUserSubmitBtn, false);
    }
  }

  document.getElementById('add-user-btn').addEventListener('click', openAddUserModal);
  document.getElementById('add-user-cancel').addEventListener('click', closeAddUserModal);
  addUserModalOverlay.addEventListener('click', (event) => {
    if (event.target === addUserModalOverlay) closeAddUserModal();
  });
  addUserSubmitBtn.addEventListener('click', submitAddUser);
  for (const fieldId of ['new-user-username', 'new-user-password']) {
    document.getElementById(fieldId).addEventListener('keydown', (e) => {
      if (e.key === 'Enter') submitAddUser();
    });
  }
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && addUserModalOverlay.classList.contains('open')) closeAddUserModal();
  });

  // ---- Edit-password modal ----
  const editPasswordModalOverlay = document.getElementById('edit-password-modal-overlay');
  const editPasswordSubtext = document.getElementById('edit-password-subtext');
  const editPasswordInput = document.getElementById('edit-password-input');
  const editPasswordError = document.getElementById('edit-password-error');
  const editPasswordSubmitBtn = document.getElementById('edit-password-submit');
  let editPasswordUsername = null;

  function openEditPasswordModal(username) {
    editPasswordUsername = username;
    editPasswordInput.value = '';
    editPasswordError.textContent = '';
    editPasswordSubtext.textContent = `Set a new password for "${username}".`;
    editPasswordModalOverlay.classList.add('open');
    editPasswordInput.focus();
  }

  function closeEditPasswordModal() {
    editPasswordModalOverlay.classList.remove('open');
    editPasswordUsername = null;
  }

  async function submitEditPassword() {
    const password = editPasswordInput.value;
    if (!password) {
      editPasswordError.textContent = 'Password is required.';
      return;
    }
    editPasswordError.textContent = '';
    setButtonBusy(editPasswordSubmitBtn, true);
    try {
      const response = await api(`/api/users/${encodeURIComponent(editPasswordUsername)}/password`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password }),
      });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        editPasswordError.textContent = body.error || 'Could not change that password.';
        return;
      }
      const username = editPasswordUsername;
      closeEditPasswordModal();
      showStatus(`Updated password for "${username}".`);
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      setButtonBusy(editPasswordSubmitBtn, false);
    }
  }

  document.getElementById('edit-password-cancel').addEventListener('click', closeEditPasswordModal);
  editPasswordModalOverlay.addEventListener('click', (event) => {
    if (event.target === editPasswordModalOverlay) closeEditPasswordModal();
  });
  editPasswordSubmitBtn.addEventListener('click', submitEditPassword);
  editPasswordInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') submitEditPassword();
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && editPasswordModalOverlay.classList.contains('open')) closeEditPasswordModal();
  });

  // ---- Rename-user modal ----
  const renameUserModalOverlay = document.getElementById('rename-user-modal-overlay');
  const renameUserSubtext = document.getElementById('rename-user-subtext');
  const renameUserInput = document.getElementById('rename-user-input');
  const renameUserError = document.getElementById('rename-user-error');
  const renameUserSubmitBtn = document.getElementById('rename-user-submit');
  let renameUserUsername = null;

  function openRenameUserModal(username) {
    renameUserUsername = username;
    renameUserInput.value = username;
    renameUserError.textContent = '';
    renameUserSubtext.textContent = `Set a new username for "${username}".`;
    renameUserModalOverlay.classList.add('open');
    renameUserInput.focus();
    renameUserInput.select();
  }

  function closeRenameUserModal() {
    renameUserModalOverlay.classList.remove('open');
    renameUserUsername = null;
  }

  async function submitRenameUser() {
    const newUsername = renameUserInput.value.trim();
    if (!newUsername) {
      renameUserError.textContent = 'Username is required.';
      return;
    }
    renameUserError.textContent = '';
    setButtonBusy(renameUserSubmitBtn, true);
    try {
      const oldUsername = renameUserUsername;
      const response = await api(`/api/users/${encodeURIComponent(oldUsername)}/username`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ new_username: newUsername }),
      });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        renameUserError.textContent = body.error || 'Could not rename that user.';
        return;
      }
      closeRenameUserModal();
      showStatus(`Renamed "${oldUsername}" to "${newUsername}".`);
      // currentUser.username is stale for anyone who just renamed themselves
      // (the server-side session was updated in place, but this tab's own
      // cached identity wasn't) — refresh it so later self-checks (e.g. the
      // edit/delete buttons' enabled state) still key off the right name.
      if (currentUser && oldUsername === currentUser.username) {
        await loadCurrentUser();
      }
      loadUsers();
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      setButtonBusy(renameUserSubmitBtn, false);
    }
  }

  document.getElementById('rename-user-cancel').addEventListener('click', closeRenameUserModal);
  renameUserModalOverlay.addEventListener('click', (event) => {
    if (event.target === renameUserModalOverlay) closeRenameUserModal();
  });
  renameUserSubmitBtn.addEventListener('click', submitRenameUser);
  renameUserInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') submitRenameUser();
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && renameUserModalOverlay.classList.contains('open')) closeRenameUserModal();
  });

  const assignGuildModalOverlay = document.getElementById('assign-guild-modal-overlay');
  const assignGuildSubtext = document.getElementById('assign-guild-subtext');
  const assignGuildSelect = document.getElementById('assign-guild-select');
  const assignGuildError = document.getElementById('assign-guild-error');
  const assignGuildSubmitBtn = document.getElementById('assign-guild-submit');
  let assignGuildUsername = null;

  function openAssignGuildModal(username, guildId) {
    assignGuildUsername = username;
    fillGuildSelect(assignGuildSelect, guildId || '');
    assignGuildError.textContent = '';
    assignGuildSubtext.textContent = `Choose which server "${username}" can see and control. Users who can manage users always reach every server.`;
    assignGuildModalOverlay.classList.add('open');
    assignGuildSelect.focus();
  }

  function closeAssignGuildModal() {
    assignGuildModalOverlay.classList.remove('open');
    assignGuildUsername = null;
  }

  async function submitAssignGuild() {
    const username = assignGuildUsername;
    const guildId = assignGuildSelect.value || null;
    assignGuildError.textContent = '';
    setButtonBusy(assignGuildSubmitBtn, true);
    try {
      const response = await api(`/api/users/${encodeURIComponent(username)}/guild`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ guild_id: guildId }),
      });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        assignGuildError.textContent = body.error || 'Could not change that server access.';
        return;
      }
      closeAssignGuildModal();
      // The server drops every session for the account it just rescoped, so
      // a change to your own account has already invalidated this tab.
      if (currentUser && username === currentUser.username) {
        showLogin('Your server access changed — please sign in again.');
        return;
      }
      showStatus(`"${username}" can now reach ${guildId ? guildLabel(guildId) : 'all servers'}.`);
      loadUsers();
    } catch (err) { /* showLogin already handled unauthorized */
    } finally {
      setButtonBusy(assignGuildSubmitBtn, false);
    }
  }

  document.getElementById('assign-guild-cancel').addEventListener('click', closeAssignGuildModal);
  assignGuildModalOverlay.addEventListener('click', (event) => {
    if (event.target === assignGuildModalOverlay) closeAssignGuildModal();
  });
  assignGuildSubmitBtn.addEventListener('click', submitAssignGuild);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && assignGuildModalOverlay.classList.contains('open')) closeAssignGuildModal();
  });

  setTabIcon(false);

  if (token) {
    showDashboard();
  }
})();
