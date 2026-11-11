<script lang="ts">
  import { onMount, tick } from "svelte";
  import { api, env, errorOf } from "../lib/api";
  import { connect, drive } from "../lib/drive.svelte";
  import Icon from "../lib/Icon.svelte";
  import type { AccountCheck, SetupResult } from "../lib/types";

  type Step = "welcome" | "account" | "encryption" | "folder" | "files" | "done";
  const NUMBERED: Step[] = ["account", "encryption", "folder", "files"];
  const windows = env().platform === "windows";
  const pc = windows ? "PC" : "computer";

  let step = $state<Step>("welcome");
  let heading = $state<HTMLHeadingElement>();
  let busy = $state(false);
  let formError = $state<string | null>(null);

  // Account
  let login = $state("");
  let password = $state("");
  let appToken = $state("");
  let loginError = $state<string | null>(null);
  let passwordError = $state<string | null>(null);
  let account = $state<AccountCheck | null>(null);

  // Encryption
  let mode = $state<"generate" | "own">("generate");
  let encPassword = $state("");
  let encConfirm = $state("");
  let showEnc = $state(false);
  let encError = $state<string | null>(null);
  let confirmError = $state<string | null>(null);

  // Folder
  let folder = $state("");
  let defaultFolder = $state("");

  // Done
  let result = $state<SetupResult | null>(null);
  let saved = $state(false);
  let savedError = $state<string | null>(null);
  let copied = $state(false);
  let kitMessage = $state<{ ok: boolean; text: string } | null>(null);

  const reconnecting = $derived(drive.status?.state === "signed_out");
  const stepNumber = $derived(NUMBERED.indexOf(step) + 1);

  onMount(async () => {
    connect({});
    try {
      defaultFolder = folder = await api.defaultFolder();
    } catch {
      /* The service picks its default when folder is null. */
    }
  });

  $effect(() => {
    if (reconnecting && step === "welcome") step = "account";
  });

  async function go(next: Step) {
    formError = null;
    step = next;
    await tick();
    heading?.focus();
  }

  function back() {
    const order: Step[] = ["welcome", ...NUMBERED];
    const i = order.indexOf(step);
    if (i > 0) go(order[i - 1]);
  }

  async function submitAccount(e: SubmitEvent) {
    e.preventDefault();
    if (busy) return;
    loginError = login.trim() ? null : "Enter your uloz.to login.";
    passwordError = password ? null : "Enter your uloz.to password.";
    if (loginError || passwordError) {
      document.getElementById(loginError ? "login" : "password")?.focus();
      return;
    }
    busy = true;
    formError = null;
    try {
      account = await api.checkAccount(login.trim(), password);
      go("encryption");
    } catch (err) {
      const { code, message } = errorOf(err);
      if (code === "auth_failed") {
        passwordError = "That login or password isn't right. Check both and try again.";
        document.getElementById("password")?.focus();
      } else if (code === "network") {
        formError = "Can't reach uloz.to. Check your internet connection and try again.";
      } else {
        formError = message;
      }
    } finally {
      busy = false;
    }
  }

  function submitEncryption(e: SubmitEvent) {
    e.preventDefault();
    encError = confirmError = null;
    if (account?.vault_exists) {
      if (!encPassword) encError = "Enter the encryption password you chose when you first set up Drive.";
    } else if (mode === "own") {
      if (encPassword.length < 12) encError = "Use at least 12 characters.";
      else if (encConfirm !== encPassword) confirmError = "The passwords don't match. Type the same password in both boxes.";
    }
    if (encError || confirmError) {
      document.getElementById(encError ? "enc" : "enc-confirm")?.focus();
      return;
    }
    go("folder");
  }

  async function chooseFolder() {
    try {
      const picked = await api.pickFolder();
      if (picked) folder = picked;
    } catch (err) {
      formError = errorOf(err).message;
    }
  }

  async function runSetup(e: SubmitEvent) {
    e.preventDefault();
    if (busy) return;
    busy = true;
    formError = null;
    const ownPassword = account?.vault_exists || mode === "own";
    try {
      result = await api.setup({
        login: login.trim(),
        password,
        encryption_password: ownPassword ? encPassword : null,
        folder: folder && folder !== defaultFolder ? folder : null,
        app_token: appToken.trim() || null,
      });
      password = "";
      go("done");
    } catch (err) {
      const { code, message } = errorOf(err);
      if (code === "wrong_encryption_password") {
        encError = "That password doesn't unlock your files. Check it and try again.";
        await go("encryption");
        document.getElementById("enc")?.focus();
      } else if (code === "auth_failed") {
        passwordError = "uloz.to didn't accept your password. Enter it again.";
        await go("account");
        document.getElementById("password")?.focus();
      } else if (code === "network") {
        formError = "Can't reach uloz.to. Check your internet connection and try again.";
      } else {
        formError = `Setup didn't finish. ${message}`;
      }
    } finally {
      busy = false;
    }
  }

  async function copyPassword() {
    const text = result?.generated_encryption_password ?? "";
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = Object.assign(document.createElement("textarea"), { value: text });
      document.body.append(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    copied = true;
    setTimeout(() => (copied = false), 2000);
  }

  async function saveKit() {
    if (busy) return;
    busy = true;
    kitMessage = null;
    try {
      const path = await api.saveRecoveryKit();
      if (path) kitMessage = { ok: true, text: `Recovery kit saved to ${path}` };
    } catch (err) {
      kitMessage = { ok: false, text: `Couldn't save the recovery kit. ${errorOf(err).message}` };
    } finally {
      busy = false;
    }
  }

  function finish(e: SubmitEvent) {
    e.preventDefault();
    if (result?.generated_encryption_password && !saved) {
      savedError = "Confirm that you've saved your encryption password.";
      document.getElementById("saved")?.focus();
      return;
    }
    api.finishOnboarding();
  }
</script>

<div class="onboarding">
  {#if stepNumber > 0}
    <div class="progress" aria-hidden="true">
      {#each NUMBERED as s, i}
        <span class:done={i < stepNumber - 1} class:current={NUMBERED[stepNumber - 1] === s}></span>
      {/each}
    </div>
  {/if}

  {#if step === "welcome"}
    <form class="step" onsubmit={(e) => (e.preventDefault(), go("account"))}>
      <div class="content center">
        <img src="/app-icon.svg" alt="" width="72" height="72" />
        <h1 class="title" tabindex="-1" bind:this={heading}>Welcome to Drive</h1>
        <p class="lead secondary">
          Drive keeps a folder on this {pc} in sync with your uloz.to account. Files are encrypted on this {pc} before
          they upload, so only you can read them.
        </p>
        <ul class="points" role="list">
          <li><Icon name="lock" /><span>End-to-end encrypted with a password only you know</span></li>
          <li><Icon name="cloud" /><span>Files take space only when you open them</span></li>
          <li><Icon name="folder" /><span>Works like any other folder</span></li>
        </ul>
      </div>
      <div class="actions">
        <span></span>
        <button class="btn primary" type="submit">Get started</button>
      </div>
    </form>
  {:else if step === "account"}
    <form class="step" onsubmit={submitAccount} novalidate>
      <div class="content">
        <p class="caption secondary">Step {stepNumber} of {NUMBERED.length}</p>
        <h1 class="title" tabindex="-1" bind:this={heading}>
          {reconnecting ? "Sign in to uloz.to again" : "Sign in to uloz.to"}
        </h1>
        <p class="secondary">
          {reconnecting
            ? "uloz.to stopped accepting the saved sign-in. Sign in to keep syncing."
            : "Drive stores your encrypted files in your uloz.to account."}
        </p>
        <div class="field">
          <label for="login">Login or email</label>
          <input
            id="login"
            class="input"
            name="username"
            autocomplete="username"
            spellcheck="false"
            bind:value={login}
            aria-invalid={loginError ? "true" : undefined}
            aria-describedby={loginError ? "login-error" : undefined}
          />
          {#if loginError}
            <p class="error-text" id="login-error"><Icon name="alert" size={14} />{loginError}</p>
          {/if}
        </div>
        <div class="field">
          <label for="password">Password</label>
          <input
            id="password"
            class="input"
            type="password"
            name="password"
            autocomplete="current-password"
            bind:value={password}
            aria-invalid={passwordError ? "true" : undefined}
            aria-describedby={passwordError ? "password-error" : undefined}
          />
          {#if passwordError}
            <p class="error-text" id="password-error"><Icon name="alert" size={14} />{passwordError}</p>
          {/if}
        </div>
        <details class="advanced">
          <summary>Use your own uloz.to API key</summary>
          <div class="field">
            <label for="token">API key (optional)</label>
            <p class="hint" id="token-hint">
              uloz.to asks apps to use their own API key. Leave this empty to use Drive's shared key. You can add a key
              later in Settings.
            </p>
            <input
              id="token"
              class="input"
              type="password"
              autocomplete="off"
              spellcheck="false"
              bind:value={appToken}
              aria-describedby="token-hint"
            />
          </div>
        </details>
        {#if formError}
          <p class="error-text" role="alert"><Icon name="alert" size={14} />{formError}</p>
        {/if}
      </div>
      <div class="actions">
        {#if reconnecting}<span></span>{:else}<button class="btn" type="button" onclick={back}>Back</button>{/if}
        <button class="btn primary" type="submit" aria-disabled={busy}>{busy ? "Checking…" : "Continue"}</button>
      </div>
    </form>
  {:else if step === "encryption"}
    <form class="step" onsubmit={submitEncryption} novalidate>
      <div class="content">
        <p class="caption secondary">Step {stepNumber} of {NUMBERED.length}</p>
        {#if account?.vault_exists}
          <h1 class="title" tabindex="-1" bind:this={heading}>Unlock your files</h1>
          <p class="secondary">
            This uloz.to account already has encrypted Drive files. Enter the encryption password you chose when you first
            set up Drive. It's different from your uloz.to password.
          </p>
          <div class="field">
            <label for="enc">Encryption password</label>
            <div class="password-row">
              <input
                id="enc"
                class="input"
                type={showEnc ? "text" : "password"}
                autocomplete="off"
                spellcheck="false"
                bind:value={encPassword}
                aria-invalid={encError ? "true" : undefined}
                aria-describedby={encError ? "enc-error" : undefined}
              />
              <button class="btn" type="button" aria-pressed={showEnc} onclick={() => (showEnc = !showEnc)}>
                <Icon name={showEnc ? "eye-off" : "eye"} size={16} />
                {showEnc ? "Hide" : "Show"}
              </button>
            </div>
            {#if encError}
              <p class="error-text" id="enc-error"><Icon name="alert" size={14} />{encError}</p>
            {/if}
          </div>
        {:else}
          <h1 class="title" tabindex="-1" bind:this={heading}>Encrypt your files</h1>
          <p class="secondary">
            Drive encrypts every file on this {pc} before it uploads. Your encryption password is the only key: uloz.to
            and the people who make Drive can't read your files, and can't recover them if you lose it.
          </p>
          <fieldset class="choices">
            <legend class="visually-hidden">Encryption password</legend>
            <label class="choice card" class:selected={mode === "generate"}>
              <input type="radio" name="mode" value="generate" bind:group={mode} />
              <span>
                <span class="body-strong">Create a strong password for me</span>
                <span class="caption secondary">Recommended. You'll save it in the next steps.</span>
              </span>
            </label>
            <label class="choice card" class:selected={mode === "own"}>
              <input type="radio" name="mode" value="own" bind:group={mode} />
              <span>
                <span class="body-strong">Choose my own password</span>
                <span class="caption secondary">Pick something long that you don't use anywhere else.</span>
              </span>
            </label>
          </fieldset>
          {#if mode === "own"}
            <div class="field">
              <label for="enc">Encryption password</label>
              <p class="hint" id="enc-hint">Use at least 12 characters. A few unrelated words work well.</p>
              <div class="password-row">
                <input
                  id="enc"
                  class="input"
                  type={showEnc ? "text" : "password"}
                  autocomplete="new-password"
                  spellcheck="false"
                  bind:value={encPassword}
                  aria-invalid={encError ? "true" : undefined}
                  aria-describedby={encError ? "enc-hint enc-error" : "enc-hint"}
                />
                <button class="btn" type="button" aria-pressed={showEnc} onclick={() => (showEnc = !showEnc)}>
                  <Icon name={showEnc ? "eye-off" : "eye"} size={16} />
                  {showEnc ? "Hide" : "Show"}
                </button>
              </div>
              {#if encError}
                <p class="error-text" id="enc-error"><Icon name="alert" size={14} />{encError}</p>
              {/if}
            </div>
            <div class="field">
              <label for="enc-confirm">Type it again</label>
              <input
                id="enc-confirm"
                class="input"
                type={showEnc ? "text" : "password"}
                autocomplete="new-password"
                spellcheck="false"
                bind:value={encConfirm}
                aria-invalid={confirmError ? "true" : undefined}
                aria-describedby={confirmError ? "confirm-error" : undefined}
              />
              {#if confirmError}
                <p class="error-text" id="confirm-error"><Icon name="alert" size={14} />{confirmError}</p>
              {/if}
            </div>
          {/if}
        {/if}
      </div>
      <div class="actions">
        <button class="btn" type="button" onclick={back}>Back</button>
        <button class="btn primary" type="submit">Continue</button>
      </div>
    </form>
  {:else if step === "folder"}
    <form class="step" onsubmit={(e) => (e.preventDefault(), go("files"))}>
      <div class="content">
        <p class="caption secondary">Step {stepNumber} of {NUMBERED.length}</p>
        <h1 class="title" tabindex="-1" bind:this={heading}>Choose where your files appear</h1>
        <p class="secondary">
          Your Drive files show up in this folder. Use a new or empty folder.
        </p>
        <div class="card folder">
          <Icon name="folder" />
          <span class="selectable path">{folder || "Default location"}</span>
          <button class="btn" type="button" onclick={chooseFolder}>Change…</button>
        </div>
        {#if folder && folder !== defaultFolder}
          <button class="link" type="button" onclick={() => (folder = defaultFolder)}>Use the default location</button>
        {/if}
        {#if formError}
          <p class="error-text" role="alert"><Icon name="alert" size={14} />{formError}</p>
        {/if}
      </div>
      <div class="actions">
        <button class="btn" type="button" onclick={back}>Back</button>
        <button class="btn primary" type="submit">Continue</button>
      </div>
    </form>
  {:else if step === "files"}
    <form class="step" onsubmit={runSetup}>
      <div class="content">
        <p class="caption secondary">Step {stepNumber} of {NUMBERED.length}</p>
        <h1 class="title" tabindex="-1" bind:this={heading}>Files take space only when you need them</h1>
        <p class="secondary">
          Everything in your Drive is listed in the folder, but only what you open is downloaded.
          {windows ? "The Status column in File Explorer shows where each file is." : ""}
        </p>
        <ul class="states" role="list">
          <li class="card">
            <Icon name="cloud" size={24} />
            <span>
              <span class="body-strong">Only in the cloud</span>
              <span class="secondary">Takes no space on this {pc}. Downloads when you open it, so you need to be online.</span>
            </span>
          </li>
          <li class="card">
            <Icon name="check-circle" size={24} class="ok" />
            <span>
              <span class="body-strong">Downloaded</span>
              <span class="secondary">
                You opened it recently. If space runs low, Drive removes this copy. The file stays in the cloud.
              </span>
            </span>
          </li>
          <li class="card">
            <Icon name="check-circle-filled" size={24} class="ok" />
            <span>
              <span class="body-strong">Always on this {pc}</span>
              <span class="secondary">
                {windows
                  ? "Right-click a file or folder and choose Always keep on this device to have it offline."
                  : "To keep a file or folder available offline, run drive pin followed by its path in a terminal."}
              </span>
            </span>
          </li>
        </ul>
        {#if busy}
          <p class="secondary" role="status">Setting up Drive. This can take a minute.</p>
        {/if}
        {#if formError}
          <p class="error-text" role="alert"><Icon name="alert" size={14} />{formError}</p>
        {/if}
      </div>
      <div class="actions">
        <button class="btn" type="button" onclick={back}>Back</button>
        <button class="btn primary" type="submit" aria-disabled={busy}>{busy ? "Setting up…" : "Set up Drive"}</button>
      </div>
    </form>
  {:else if step === "done" && result}
    <form class="step" onsubmit={finish} novalidate>
      <div class="content">
        {#if result.generated_encryption_password}
          <h1 class="title" tabindex="-1" bind:this={heading}>Save your encryption password</h1>
          <p class="secondary">
            Drive created this password to encrypt your files. You need it to set up Drive on another device or to
            recover your files. It's shown only this once.
          </p>
          <div class="card secret">
            <code class="selectable numeric">{result.generated_encryption_password}</code>
            <button class="btn" type="button" onclick={copyPassword}>
              <span class="swap" class:on={copied}>
                <Icon name="copy" size={16} />
                <Icon name="check" size={16} />
              </span>
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
          <p class="visually-hidden" role="status">{copied ? "Password copied" : ""}</p>
          <div class="callout warning">
            <Icon name="lock" size={18} />
            <div>
              <p>
                If you lose this password, nobody can recover your files. Keep it in a password manager, or save the
                recovery kit and store it somewhere safe.
              </p>
            </div>
          </div>
        {:else}
          <h1 class="title" tabindex="-1" bind:this={heading}>You're all set</h1>
          <p class="secondary">
            Your files appear in <span class="selectable path">{result.folder}</span>. They download when you open them.
          </p>
          {#if result.created_vault}
            <div class="callout warning">
              <Icon name="lock" size={18} />
              <div>
                <p>
                  Keep your encryption password somewhere safe. If you lose it, nobody can recover your files. A recovery
                  kit helps you keep it safe.
                </p>
              </div>
            </div>
          {/if}
        {/if}

        <div>
          <button class="btn" type="button" aria-disabled={busy} onclick={saveKit}>
            {busy ? "Preparing…" : "Save recovery kit…"}
          </button>
        </div>
        <p class="caption selectable" class:error-text={kitMessage && !kitMessage.ok} role="status">{kitMessage?.text ?? ""}</p>

        {#if result.generated_encryption_password}
          <div class="field">
            <label class="check">
              <input
                id="saved"
                type="checkbox"
                bind:checked={saved}
                onchange={() => (savedError = null)}
                aria-invalid={savedError ? "true" : undefined}
                aria-describedby={savedError ? "saved-error" : undefined}
              />
              <span>I've saved my encryption password somewhere safe</span>
            </label>
            {#if savedError}
              <p class="error-text" id="saved-error"><Icon name="alert" size={14} />{savedError}</p>
            {/if}
          </div>
        {/if}
      </div>
      <div class="actions">
        <button class="btn" type="button" onclick={() => api.openFolder()}>
          <Icon name="folder" size={16} />
          Open Drive folder
        </button>
        <button class="btn primary" type="submit">Done</button>
      </div>
    </form>
  {/if}
</div>

<style>
  .onboarding {
    display: flex;
    flex-direction: column;
    height: 100%;
  }
  .progress {
    display: flex;
    gap: var(--s1);
    padding: var(--s4) var(--s8) 0;
  }
  .progress span {
    flex: 1;
    height: 3px;
    border-radius: 2px;
    background: var(--track);
  }
  .progress .done,
  .progress .current {
    background: var(--accent);
  }

  .step {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  .content {
    display: flex;
    flex-direction: column;
    gap: var(--s4);
    flex: 1;
    width: 100%;
    max-width: 520px;
    margin-inline: auto;
    padding: var(--s6) var(--s8);
    overflow-y: auto;
    scrollbar-width: thin;
  }
  .content > .caption:first-child {
    margin-block-end: calc(var(--s3) * -1);
  }
  .center {
    align-items: center;
    justify-content: center;
    text-align: center;
  }
  .lead {
    max-width: 44ch;
  }

  .points {
    display: grid;
    gap: var(--s3);
    margin: var(--s2) 0 0;
    padding: 0;
    list-style: none;
    text-align: start;
  }
  .points li {
    display: flex;
    gap: var(--s3);
    align-items: center;
  }
  .points :global(.icon) {
    color: var(--accent-text);
  }

  .actions {
    display: flex;
    justify-content: space-between;
    gap: var(--s2);
    padding: var(--s4) var(--s8);
    border-block-start: 1px solid var(--stroke-divider);
    background: var(--surface);
  }
  .actions .btn {
    min-width: 120px;
  }

  .advanced summary {
    width: fit-content;
    color: var(--accent-text);
    cursor: pointer;
  }
  .advanced[open] summary {
    margin-block-end: var(--s3);
  }

  .password-row {
    display: flex;
    gap: var(--s2);
  }
  .password-row .input {
    flex: 1;
  }

  .choices {
    display: grid;
    gap: var(--s2);
    margin: 0;
    padding: 0;
    border: 0;
  }
  .choice {
    display: flex;
    gap: var(--s3);
    align-items: flex-start;
    padding: var(--s3) var(--s4);
  }
  .choice input {
    margin: 3px 0 0;
  }
  .choice > span {
    display: grid;
  }
  .choice.selected {
    border-color: var(--accent);
  }

  .folder {
    display: flex;
    align-items: center;
    gap: var(--s3);
    padding: var(--s3) var(--s4);
  }
  .folder > :global(.icon) {
    color: var(--text-2);
  }
  .folder .path {
    flex: 1;
    min-width: 0;
  }
  .path {
    overflow-wrap: anywhere;
  }

  .states {
    display: grid;
    gap: var(--s2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .states li {
    display: flex;
    gap: var(--s3);
    align-items: flex-start;
    padding: var(--s3) var(--s4);
  }
  .states li > span {
    display: grid;
    gap: 2px;
  }
  .states :global(.icon) {
    color: var(--text-2);
  }
  .states :global(.icon.ok) {
    color: var(--success-text);
  }

  .secret {
    display: flex;
    align-items: center;
    gap: var(--s3);
    padding: var(--s4);
  }
  .secret code {
    flex: 1;
    min-width: 0;
    font-family: var(--font-mono);
    font-size: 1.125rem;
    line-height: 1.4;
    letter-spacing: 0.02em;
    overflow-wrap: anywhere;
  }

  .swap {
    display: grid;
  }
  .swap > :global(*) {
    grid-area: 1 / 1;
    transition: opacity 300ms cubic-bezier(0.2, 0, 0, 1);
  }
  .swap > :global(:last-child),
  .swap.on > :global(:first-child) {
    opacity: 0;
  }
  .swap.on > :global(:last-child) {
    opacity: 1;
  }
  @media (prefers-reduced-motion: no-preference) {
    .swap > :global(*) {
      transition-property: opacity, scale, filter;
    }
    .swap > :global(:last-child),
    .swap.on > :global(:first-child) {
      scale: 0.25;
      filter: blur(4px);
    }
    .swap.on > :global(:last-child) {
      scale: 1;
      filter: blur(0);
    }
  }

  .check {
    display: flex;
    gap: var(--s2);
    align-items: flex-start;
    font-weight: 600;
  }
  .check input {
    width: 16px;
    height: 16px;
    margin: 2px 0 0;
  }

  @media (max-width: 520px) {
    .content,
    .actions {
      padding-inline: var(--s4);
    }
  }
</style>
