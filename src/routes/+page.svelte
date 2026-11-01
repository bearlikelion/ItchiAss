<script lang="ts">
  import { onMount } from "svelte";
  import { SvelteMap, SvelteSet } from "svelte/reactivity";
  import { open } from "@tauri-apps/plugin-dialog";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
  import { check, type Update } from "@tauri-apps/plugin-updater";
  import { relaunch } from "@tauri-apps/plugin-process";
  import * as api from "$lib/api";
  import type { Game, Progress, Settings, Status, UploadFile } from "$lib/api";

  let settings = $state<Settings | null>(null);
  let games = $state<Game[]>([]);
  let busy = $state("");
  let error = $state("");
  let apiKey = $state("");
  let showSettings = $state(false);
  let loaded = $state(false);
  let appUpdate = $state<Update | null>(null);
  let installing = $state(false);
  let expanded = $state<number | null>(null);
  let files = $state<UploadFile[] | null>(null);
  let filesError = $state("");

  let q = $state("");
  let fClass = $state("");
  let fCreator = $state("");
  let fStatus = $state("");
  let fPlatform = $state("");

  const selected = new SvelteSet<number>();
  const progress = new SvelteMap<string, Progress>();

  const classes = $derived([...new Set(games.map((g) => g.classification))].sort());
  const creators = $derived([...new Set(games.map((g) => g.creator))].sort((a, b) => a.localeCompare(b)));

  const filtered = $derived.by(() => {
    const needle = q.trim().toLowerCase();
    return games.filter(
      (g) =>
        (!needle || g.title.toLowerCase().includes(needle) || g.creator.toLowerCase().includes(needle)) &&
        (!fClass || g.classification === fClass) &&
        (!fCreator || g.creator === fCreator) &&
        (!fStatus || g.status === fStatus) &&
        (!fPlatform || g.platforms.includes(fPlatform)),
    );
  });

  const filtering = $derived(!!(q.trim() || fClass || fCreator || fStatus || fPlatform));
  const owned = $derived(games.filter((g) => g.status !== "none"));
  const allSelected = $derived(filtered.length > 0 && filtered.every((g) => selected.has(g.game_id)));
  const updates = $derived(games.filter((g) => g.status === "update").length);
  const active = $derived([...progress.values()].filter((p) => p.upload_id !== 0 || p.state === "error"));
  const running = $derived(active.filter((p) => p.state === "downloading" || p.state === "extracting").length);
  const cancellable = $derived(active.filter((p) => p.state === "queued" || p.state === "downloading"));
  let wasBusy = false;
  $effect(() => {
    const busyNow = cancellable.length > 0 || running > 0;
    if (wasBusy && !busyNow) {
      const failed = active.filter((p) => p.state === "error").length;
      const done = active.filter((p) => p.state === "done").length;
      notify("Downloads finished", failed ? `${done} done, ${failed} failed` : `${done} done`);
    }
    wasBusy = busyNow;
  });

  async function run<T>(label: string, fn: () => Promise<T>): Promise<T | undefined> {
    busy = label;
    error = "";
    try {
      return await fn();
    } catch (e) {
      error = String(e);
    } finally {
      busy = "";
    }
  }

  async function refreshSettings() {
    settings = await api.getSettings();
    if (!settings.logged_in) return;
    await loadLibrary();
    if (settings.check_on_launch && owned.length) {
      await checkUpdates();
      if (updates) notify("Updates available", `${updates} item${updates === 1 ? " has" : "s have"} a newer version.`);
    }
  }

  async function loadLibrary() {
    if (!games.length) {
      const cached = await api.cachedLibrary();
      if (cached.length) {
        games = cached;
        loaded = true;
      }
    }
    const lib = await run(games.length ? "Refreshing library…" : "Loading library…", api.library);
    if (!lib) return;
    const statuses = new Map(games.map((g) => [g.game_id, g.status]));
    games = lib.map((g) => (statuses.get(g.game_id) === "update" ? { ...g, status: "update" } : g));
    loaded = true;
  }

  async function notify(title: string, body: string) {
    try {
      if (!(await isPermissionGranted()) && (await requestPermission()) !== "granted") return;
      sendNotification({ title, body });
    } catch {
      // Notifications are best effort.
    }
  }

  async function checkAppUpdate() {
    try {
      appUpdate = await check();
    } catch {
      appUpdate = null;
    }
  }

  async function installAppUpdate() {
    if (!appUpdate) return;
    installing = true;
    try {
      await appUpdate.downloadAndInstall();
      await relaunch();
    } catch (e) {
      error = `Update failed: ${e}`;
      installing = false;
    }
  }

  async function doLogin() {
    await run("Waiting for itch.io in your browser…", api.login);
    await refreshSettings();
  }

  async function doApiKey() {
    const ok = await run("Checking key…", async () => (await api.setApiKey(apiKey), true));
    if (!ok) return;
    apiKey = "";
    await refreshSettings();
  }

  async function doLogout() {
    await api.logout();
    games = [];
    loaded = false;
    selected.clear();
    await refreshSettings();
  }

  async function persist() {
    if (settings) await run("", () => api.saveSettings(settings!));
  }

  async function pickFolder() {
    const dir = await open({ directory: true, defaultPath: settings?.download_dir });
    if (typeof dir === "string" && settings) {
      settings.download_dir = dir;
      await persist();
      await loadLibrary();
    }
  }

  async function checkUpdates() {
    const res = await run(`Checking ${owned.length} items for updates…`, () => api.checkUpdates(owned));
    if (!res) return;
    for (const g of games) {
      const s = res[g.game_id];
      if (s) g.status = s;
    }
  }

  function download(list: Game[]) {
    if (!list.length) return;
    const ids = new Set(list.map((g) => g.game_id));
    for (const [k, p] of progress) if (ids.has(p.game_id)) progress.delete(k);
    api.download(list).catch((e) => (error = String(e)));
  }

  function downloadSelected() {
    download(games.filter((g) => selected.has(g.game_id)));
    selected.clear();
  }

  async function toggleFiles(g: Game) {
    if (expanded === g.game_id) {
      expanded = null;
      return;
    }
    expanded = g.game_id;
    files = null;
    filesError = "";
    try {
      const list = await api.uploads(g);
      if (expanded === g.game_id) files = list;
    } catch (e) {
      filesError = String(e);
    }
  }

  async function toggleSkip(f: UploadFile) {
    f.skipped = !f.skipped;
    await api.setSkipped(f.id, f.skipped).catch((e) => (error = String(e)));
  }

  async function openFolder(g: Game) {
    await api.openFolder(g.game_id).catch((e) => (error = String(e)));
  }

  function cancelDownloads(list: Progress[]) {
    api.cancel(list.map((p) => p.upload_id));
  }

  function clearFilters() {
    q = fClass = fCreator = fStatus = fPlatform = "";
  }

  function gameError(gameId: number): string | undefined {
    return [...progress.values()].find((p) => p.game_id === gameId && p.state === "error")?.error ?? undefined;
  }

  function toggleAll() {
    if (allSelected) filtered.forEach((g) => selected.delete(g.game_id));
    else filtered.forEach((g) => selected.add(g.game_id));
  }

  function setStatus(gameId: number, status: Status) {
    const g = games.find((x) => x.game_id === gameId);
    if (g) g.status = status;
  }

  function gameState(gameId: number): string {
    const ps = [...progress.values()].filter((p) => p.game_id === gameId);
    if (ps.some((p) => p.state === "error")) return "error";
    if (ps.some((p) => p.state === "downloading" || p.state === "extracting")) return "downloading";
    if (ps.some((p) => p.state === "queued")) return "queued";
    return "";
  }

  function onProgress(p: Progress) {
    progress.set(`${p.game_id}:${p.upload_id}`, p);
    if (p.state !== "done") return;
    const pending = [...progress.values()].some(
      (x) => x.game_id === p.game_id && (x.state === "queued" || x.state === "downloading" || x.state === "extracting"),
    );
    if (!pending) setStatus(p.game_id, "downloaded");
  }

  function clearFinished() {
    for (const [k, p] of progress) if (p.state === "done" || p.state === "error" || p.state === "cancelled") progress.delete(k);
  }

  const mb = (n: number) => (n / 1048576).toFixed(1);
  const size = (n: number | null) => (n == null ? "" : n >= 1073741824 ? `${(n / 1073741824).toFixed(1)} GB` : `${mb(n)} MB`);

  onMount(() => {
    const un = api.onProgress(onProgress);
    refreshSettings();
    checkAppUpdate();
    return () => un.then((f) => f());
  });
</script>

<main>
  {#if !settings}
    <p class="empty muted">Starting…</p>
  {:else if !settings.logged_in}
    <section class="login">
      <img class="login-logo" src="/icon.svg" alt="" width="72" height="72" />
      <h1>ItchiAss<span class="ets">(ets)</span></h1>
      <p class="muted">Your itch.io library, downloaded and kept up to date.</p>
      <button class="primary" onclick={doLogin} disabled={!!busy}>Log in with itch.io</button>
      <div class="or">or paste an API key</div>
      <form onsubmit={(e) => (e.preventDefault(), doApiKey())}>
        <input type="password" bind:value={apiKey} placeholder="itch.io API key" />
        <button disabled={!apiKey || !!busy}>Use key</button>
      </form>
      <p class="muted small">
        Keys live at <button class="link" onclick={() => openUrl("https://itch.io/user/settings/api-keys")}>itch.io/user/settings/api-keys</button>
      </p>
      {#if busy}<p class="muted">{busy}</p>{/if}
      {#if error}<p class="error">{error}</p>{/if}
    </section>
  {:else}
    <header>
      {#if appUpdate}
        <div class="banner" role="status">
          <span>ItchiAss(ets) {appUpdate.version} is available.</span>
          <button class="primary" onclick={installAppUpdate} disabled={installing}>{installing ? "Installing…" : "Install and restart"}</button>
        </div>
      {/if}
      <div class="appbar">
        <div class="identity">
          <strong class="brand"><img class="logo" src="/icon.svg" alt="" width="24" height="24" /><span>ItchiAss<span class="ets">(ets)</span></span></strong>
          <span class="muted num" role="status">{busy || `${games.length} owned${updates ? ` · ${updates} with updates` : ""}`}</span>
        </div>
        <div class="cluster">
          <button onclick={checkUpdates} disabled={!!busy || !owned.length} title={owned.length ? "" : "Nothing downloaded yet"}>Check updates</button>
          <button onclick={loadLibrary} disabled={!!busy}>Refresh</button>
          <button class="ghost" aria-expanded={showSettings} aria-controls="settings" onclick={() => (showSettings = !showSettings)}>Settings</button>
          <button class="ghost" onclick={doLogout}>Log out</button>
        </div>
      </div>

      {#if showSettings}
        <div class="settings" id="settings">
          <span class="field-label">Save to</span>
          <button class="folder" onclick={pickFolder} title="Change download folder">
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M1.5 4.5v8a1 1 0 0 0 1 1h11a1 1 0 0 0 1-1v-6a1 1 0 0 0-1-1H8L6.5 3.5h-4a1 1 0 0 0-1 1Z" /></svg>
            <span>{settings.download_dir}</span>
          </button>
          <label>Parallel downloads <input type="number" min="1" max="16" bind:value={settings.parallel} onchange={persist} /></label>
          <label><input type="checkbox" bind:checked={settings.by_creator} onchange={persist} /> Sub-folder per creator</label>
          <label><input type="checkbox" bind:checked={settings.check_on_launch} onchange={persist} /> Check for updates on launch</label>
          <div class="pair">
            <label><input type="checkbox" bind:checked={settings.extract} onchange={persist} /> Extract archives</label>
            <label class:disabled={!settings.extract}>
              <input type="checkbox" bind:checked={settings.cleanup} onchange={persist} disabled={!settings.extract} /> Delete archive after extract
            </label>
          </div>
        </div>
      {/if}

      <div class="taskbar">
        <div class="filters">
          <input class="search" type="search" bind:value={q} placeholder="Search title or creator" aria-label="Search" />
          <select bind:value={fClass} aria-label="Type">
            <option value="">All types</option>
            {#each classes as c (c)}<option value={c}>{c}</option>{/each}
          </select>
          <select bind:value={fCreator} aria-label="Creator">
            <option value="">All creators</option>
            {#each creators as c (c)}<option value={c}>{c}</option>{/each}
          </select>
          <select bind:value={fStatus} aria-label="Status">
            <option value="">Any status</option>
            <option value="none">Not downloaded</option>
            <option value="downloaded">Downloaded</option>
            <option value="update">Update available</option>
          </select>
          <select bind:value={fPlatform} aria-label="Platform">
            <option value="">Any platform</option>
            <option value="windows">Windows</option>
            <option value="linux">Linux</option>
            <option value="mac">macOS</option>
            <option value="android">Android</option>
          </select>
        </div>
        <div class="cluster">
          {#if filtering}<button class="ghost" onclick={clearFilters}>Clear filters</button>{/if}
          <span class="muted num">{filtered.length} shown{selected.size ? ` · ${selected.size} selected` : ""}</span>
          <button class="primary" disabled={!selected.size} onclick={downloadSelected}>Download selected</button>
        </div>
      </div>

      {#if error}
        <div class="alert" role="alert">
          <span>{error}</span>
          <button class="ghost" onclick={() => (error = "")}>Dismiss</button>
        </div>
      {/if}
    </header>

    <div class="table">
      {#if games.length}
        <table>
          <thead>
            <tr>
              <th class="check"><input type="checkbox" checked={allSelected} onchange={toggleAll} aria-label="Select all shown" /></th>
              <th class="cover"><span class="visually-hidden">Cover</span></th>
              <th>Title</th>
              <th>Creator</th>
              <th>Type</th>
              <th>Status</th>
              <th class="action"><span class="visually-hidden">Action</span></th>
            </tr>
          </thead>
          <tbody>
            {#each filtered as g (g.game_id)}
              {@const live = gameState(g.game_id)}
              <tr class:selected={selected.has(g.game_id)}>
                <td class="check">
                  <input
                    type="checkbox"
                    checked={selected.has(g.game_id)}
                    onchange={() => (selected.has(g.game_id) ? selected.delete(g.game_id) : selected.add(g.game_id))}
                    aria-label="Select {g.title}"
                  />
                </td>
                <td class="cover"><div class="thumb">{#if g.cover_url}<img src={g.cover_url} alt="" width="52" height="41" loading="lazy" />{/if}</div></td>
                <td class="title">
                  {#if g.url}<button class="link" onclick={() => openUrl(g.url!)}>{g.title}</button>{:else}{g.title}{/if}
                </td>
                <td class="muted">{g.creator}</td>
                <td class="muted">{g.classification}</td>
                <td>
                  {#if live}
                    <span class="badge {live}" title={live === "error" ? gameError(g.game_id) : undefined}>{live === "error" ? "failed" : live}</span>
                  {:else}
                    <span class="badge {g.status}">{g.status === "none" ? "not downloaded" : g.status === "update" ? "update available" : "downloaded"}</span>
                  {/if}
                </td>
                <td class="action">
                  <div class="row-actions">
                    <button class="ghost" aria-expanded={expanded === g.game_id} onclick={() => toggleFiles(g)}>Files</button>
                    <button
                      class="ghost icon"
                      class:placeholder={g.status === "none"}
                      onclick={() => openFolder(g)}
                      title="Show in folder"
                      aria-label="Show {g.title} in folder"
                      aria-hidden={g.status === "none"}
                      tabindex={g.status === "none" ? -1 : 0}
                    >
                      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M1.5 4.5v8a1 1 0 0 0 1 1h11a1 1 0 0 0 1-1v-6a1 1 0 0 0-1-1H8L6.5 3.5h-4a1 1 0 0 0-1 1Z" /></svg>
                    </button>
                    <button class="dl" onclick={() => download([g])} disabled={live === "downloading" || live === "queued"}>
                      {g.status === "update" ? "Update" : g.status === "downloaded" ? "Sync" : "Download"}
                    </button>
                  </div>
                </td>
              </tr>
              {#if expanded === g.game_id}
                <tr class="files-row">
                  <td></td>
                  <td colspan="6">
                    {#if filesError}
                      <p class="error small">{filesError}</p>
                    {:else if !files}
                      <p class="muted small">Loading files…</p>
                    {:else if !files.length}
                      <p class="muted small">This item has no downloadable files.</p>
                    {:else}
                      <p class="muted small">Unchecked files are skipped by Download, Sync and Check updates.</p>
                      <ul class="files">
                        {#each files as f (f.id)}
                          <li>
                            <label>
                              <input type="checkbox" checked={!f.skipped} onchange={() => toggleSkip(f)} />
                              <span class="fname">{f.display_name || f.filename}</span>
                            </label>
                            <span class="muted num">{size(f.size)}</span>
                            {#if f.downloaded}<span class="badge downloaded">downloaded</span>{/if}
                          </li>
                        {/each}
                      </ul>
                    {/if}
                  </td>
                </tr>
              {/if}
            {/each}
          </tbody>
        </table>
      {/if}
      {#if !filtered.length && !busy}
        <div class="empty muted">
          {#if games.length}
            <p>Nothing matches these filters.</p>
            <button onclick={clearFilters}>Clear filters</button>
          {:else if loaded}
            <p>Your itch.io library is empty.</p>
          {:else}
            <p>Couldn't load your library.</p>
            <button onclick={loadLibrary}>Try again</button>
          {/if}
        </div>
      {/if}
    </div>

    {#if active.length}
      <footer>
        <div class="footbar">
          <strong>Downloads</strong>
          <span class="muted num">{running} active{cancellable.length > running ? ` · ${cancellable.length - running} queued` : ""}</span>
          <span class="footbar-actions">
            {#if cancellable.length}<button class="ghost" onclick={() => cancelDownloads(cancellable)}>Cancel all</button>{/if}
            <button class="ghost" onclick={clearFinished}>Clear finished</button>
          </span>
        </div>
        <ul>
          {#each active as p (`${p.game_id}:${p.upload_id}`)}
            <li class={p.state}>
              <span class="name">{p.filename || games.find((g) => g.game_id === p.game_id)?.title}</span>
              <progress max={p.total || 1} value={p.state === "done" ? p.total || 1 : p.received}></progress>
              <span class="info num">
                {#if p.state === "error"}{p.error}{:else if p.state === "downloading"}{mb(p.received)} / {mb(p.total)} MB{:else if p.state === "queued" && p.error}{p.error}{:else if p.state === "cancelled"}cancelled, resumes next time{:else}{p.state}{/if}
              </span>
              {#if p.state === "queued" || p.state === "downloading"}
                <button class="ghost small-btn" onclick={() => cancelDownloads([p])} aria-label="Cancel {p.filename}">Cancel</button>
              {:else}
                <span></span>
              {/if}
            </li>
          {/each}
        </ul>
      </footer>
    {/if}
  {/if}
</main>

<style>
  :global(:root) {
    --bg: #f7f7f8;
    --panel: #ffffff;
    --raised: #fbfbfc;
    --fg: #1b1b1f;
    --muted: #63636e;
    --line: #e2e2e7;
    --accent: #d63c43;
    --accent-fg: #ffffff;
    --tint: #fdeeee;
    --ok: #237a45;
    --warn: #9a5c00;
    --bad: #be2a5c;
    --s1: 4px;
    --s2: 8px;
    --s3: 12px;
    --s4: 16px;
    --s5: 24px;
    --gutter: var(--s4);
    color-scheme: light dark;
  }
  @media (prefers-color-scheme: dark) {
    :global(:root) {
      --bg: #17171a;
      --panel: #1f1f23;
      --raised: #232328;
      --fg: #ececf1;
      --muted: #a3a3ae;
      --line: #313137;
      --accent: #fa5c5c;
      --accent-fg: #17171a;
      --tint: #3a2124;
      --ok: #5cc98a;
      --warn: #f0aa3c;
      --bad: #f06595;
    }
  }
  :global(body) {
    margin: 0;
    background: var(--bg);
    color: var(--fg);
    font: 14px/1.4 system-ui, sans-serif;
    scrollbar-color: var(--line) transparent;
  }
  :global(::selection) {
    background: var(--accent);
    color: var(--accent-fg);
  }
  :global(:focus-visible) {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  main {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  /* header: identity, optional settings, then the task bar */
  header {
    background: var(--panel);
    border-bottom: 1px solid var(--line);
    padding: var(--s3) var(--gutter);
    display: flex;
    flex-direction: column;
    gap: var(--s3);
  }
  .appbar,
  .taskbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s2) var(--s5);
    flex-wrap: wrap;
  }
  .identity {
    display: flex;
    align-items: center;
    gap: var(--s3);
  }
  .brand {
    display: inline-flex;
    align-items: center;
    gap: var(--s2);
    color: var(--accent);
    font-size: 18px;
    letter-spacing: -0.01em;
  }
  .logo {
    width: 24px;
    height: 24px;
    display: block;
  }
  .login-logo {
    width: 72px;
    height: 72px;
    align-self: center;
  }
  .ets {
    font-weight: 400;
    opacity: 0.7;
  }
  .cluster,
  .filters {
    display: flex;
    align-items: center;
    gap: var(--s2);
    flex-wrap: wrap;
  }
  .filters {
    flex: 1 1 44rem;
    flex-wrap: nowrap;
    min-width: 0;
  }
  .taskbar > .cluster {
    margin-left: auto;
  }
  .search {
    flex: 1 1 12rem;
    min-width: 9rem;
  }
  .filters select {
    flex: 0 1 auto;
    min-width: 0;
  }
  .settings {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--s2) var(--s5);
    padding: var(--s3);
    background: var(--raised);
    border: 1px solid var(--line);
    border-radius: 8px;
  }
  .pair {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s2) var(--s4);
  }
  .field-label {
    color: var(--muted);
    margin-right: calc(var(--s2) - var(--s5));
  }
  .alert {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s3);
    padding: var(--s1) var(--s1) var(--s1) var(--s3);
    border: 1px solid var(--bad);
    border-radius: 6px;
    color: var(--bad);
    font-size: 13px;
  }
  .alert span {
    overflow-wrap: anywhere;
  }
  .num {
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .muted {
    color: var(--muted);
  }
  .error {
    color: var(--bad);
  }
  .disabled {
    opacity: 0.5;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }

  /* controls */
  button,
  input,
  select {
    font: inherit;
    color: inherit;
    background: var(--bg);
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 5px 10px;
  }
  button {
    cursor: pointer;
    transition:
      border-color 120ms ease-out,
      background-color 120ms ease-out,
      color 120ms ease-out;
  }
  button:active:not(:disabled) {
    transform: translateY(1px);
  }
  button:hover:not(:disabled) {
    border-color: var(--muted);
  }
  button:disabled {
    opacity: 0.45;
    cursor: default;
  }
  button.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--accent-fg);
    font-weight: 600;
  }
  button.primary:disabled {
    background: var(--bg);
    border-color: var(--line);
    color: var(--muted);
    opacity: 1;
  }
  button.ghost {
    background: none;
    border-color: transparent;
    color: var(--muted);
  }
  button.ghost:hover,
  button.ghost[aria-expanded="true"] {
    color: var(--fg);
    border-color: var(--line);
  }
  button.link {
    border: 0;
    background: none;
    padding: 0;
    text-align: left;
    font-weight: 500;
  }
  button.link:hover {
    color: var(--accent);
    text-decoration: underline;
    text-underline-offset: 3px;
  }
  button.folder {
    display: flex;
    align-items: center;
    gap: var(--s2);
    max-width: min(48ch, 100%);
  }
  button.folder span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  button.folder svg {
    flex: none;
    width: 16px;
    height: 16px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.3;
    stroke-linejoin: round;
  }
  input[type="number"] {
    width: 4em;
  }
  input[type="checkbox"] {
    padding: 0;
    margin: 0;
    width: 16px;
    height: 16px;
    accent-color: var(--accent);
  }
  label {
    display: flex;
    align-items: center;
    gap: var(--s2);
  }

  /* library table */
  .table {
    flex: 1;
    overflow: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
  }
  th {
    position: sticky;
    top: 0;
    z-index: 1;
    background: var(--panel);
    text-align: left;
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
    border-bottom: 1px solid var(--line);
  }
  th,
  td {
    padding: var(--s2) var(--s3);
    border-bottom: 1px solid var(--line);
  }
  th:first-child,
  td:first-child {
    padding-left: var(--gutter);
  }
  th:last-child,
  td:last-child {
    padding-right: var(--gutter);
  }
  .check {
    width: 16px;
  }
  .check input {
    display: block;
  }
  .cover {
    width: 52px;
    padding-left: 0;
    padding-right: 0;
  }
  .thumb {
    width: 52px;
    height: 41px;
    border-radius: 4px;
    overflow: hidden;
    background: var(--line);
  }
  .thumb img {
    width: 52px;
    height: 41px;
    object-fit: cover;
    display: block;
  }
  .title {
    width: 40%;
  }
  .action {
    text-align: right;
    white-space: nowrap;
  }
  .row-actions {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: var(--s1);
  }
  .row-actions .dl {
    min-width: 6.5rem;
  }
  button.icon {
    padding: 5px 7px;
    display: flex;
  }
  button.icon svg {
    width: 16px;
    height: 16px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.3;
    stroke-linejoin: round;
  }
  .placeholder {
    visibility: hidden;
  }
  .small-btn {
    font-size: 12px;
    padding: 2px 8px;
    justify-self: end;
  }
  .files-row td {
    background: var(--raised);
  }
  .files-row .small {
    margin-bottom: var(--s2);
  }
  .files {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--s2);
  }
  .files li {
    display: flex;
    align-items: center;
    gap: var(--s3);
  }
  .files label {
    min-width: 0;
  }
  .fname {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s3);
    padding: var(--s2) var(--s2) var(--s2) var(--s3);
    background: var(--tint);
    border-radius: 6px;
  }
  tbody tr:hover {
    background: var(--raised);
  }
  tbody tr.selected,
  tbody tr.selected:hover {
    background: var(--tint);
  }
  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--s3);
    padding: var(--s5) var(--gutter);
  }
  .empty p {
    margin: 0;
  }
  .badge {
    font-size: 12px;
    padding: 1px var(--s2);
    border-radius: 999px;
    border: 1px solid currentColor;
    white-space: nowrap;
  }
  .badge.none {
    color: var(--muted);
  }
  .badge.downloaded {
    color: var(--ok);
  }
  .badge.update,
  .badge.queued,
  .badge.downloading {
    color: var(--warn);
  }
  .badge.error {
    color: var(--bad);
  }

  /* downloads tray */
  footer {
    background: var(--panel);
    border-top: 1px solid var(--line);
    padding: var(--s3) var(--gutter);
    display: flex;
    flex-direction: column;
    gap: var(--s2);
    max-height: 30vh;
    overflow: auto;
  }
  .footbar {
    display: flex;
    align-items: baseline;
    gap: var(--s3);
  }
  .footbar-actions {
    margin-left: auto;
    display: flex;
    gap: var(--s1);
  }
  footer ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--s1);
  }
  footer li {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 12rem minmax(0, 1fr) 5rem;
    gap: var(--s3);
    align-items: center;
  }
  .name,
  .info {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .info {
    font-size: 12px;
    color: var(--muted);
  }
  footer li.error .info,
  footer li.error .name {
    color: var(--bad);
  }
  progress {
    width: 100%;
    accent-color: var(--accent);
  }

  /* login */
  .login {
    margin: auto;
    width: min(360px, 100% - 2 * var(--gutter));
    display: flex;
    flex-direction: column;
    gap: var(--s3);
    text-align: center;
  }
  .login h1 {
    color: var(--accent);
    margin: 0;
  }
  .login form {
    display: flex;
    gap: var(--s2);
  }
  .login form input {
    flex: 1;
  }
  .or {
    color: var(--muted);
    font-size: 12px;
    margin-top: var(--s3);
  }
  .small {
    font-size: 12px;
    margin: 0;
  }

  @media (max-width: 720px) {
    th:nth-child(5),
    td:nth-child(5) {
      display: none;
    }
    .filters {
      display: grid;
      grid-template-columns: 1fr 1fr;
      flex-basis: 100%;
    }
    .search {
      grid-column: 1 / -1;
    }
    .taskbar > .cluster {
      flex: 1;
      justify-content: space-between;
    }
    footer ul {
      gap: var(--s2);
    }
    footer li {
      grid-template-columns: minmax(0, 1fr) 6rem 4.5rem;
      row-gap: 0;
    }
    footer li .info {
      grid-column: 1 / -1;
    }
  }
</style>
