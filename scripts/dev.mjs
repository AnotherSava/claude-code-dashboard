#!/usr/bin/env node
// `npm run tauri …` goes through here so a dev run binds one agreed port rather than a number written
// into this repo.
//
// A dev run needs that number in two launch-time places: Vite binds it (vite.config.ts reads PORT) and
// the Tauri CLI has to point the webview at the same place, which is `build.devUrl`. That field is
// static, so it cannot be the source — it is overridden per run through `--config`, and the value
// committed in tauri.conf.json is only ever read by someone who bypasses this script. Every other
// subcommand (build, icon) binds nothing and passes straight through with no port resolved.
//
// Three sources, in this order, and the run says which one answered:
//   1. PORT already in the environment — the explicit override, validated here.
//   2. The ports registry, via the dotfiles' shared dev-port.mjs. This machine's authority.
//   3. The kernel, via a listen(0) probe — only when no registry is installed at all, which is the
//      state of a clone made by someone who does not have the dotfiles.
//
// The third is a fallback, so it is bounded by one rule: it fires on ABSENCE only. A registry that is
// present and then fails — unreadable, half-pulled, a dangling symlink, an allocate refusal — stops the
// run loudly, because reporting "no registry here" about a machine that has one would bind an
// unallocated port under a false sentence. `~/.claude/skills` is itself a symlink on the author's
// machine, so a moved dotfiles checkout is exactly that case and is why lstat and stat are both
// consulted rather than existsSync.

import { spawnSync } from 'node:child_process'
import { createServer } from 'node:net'
import { lstatSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { homedir, tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const USE_CASE = 'tauri-dashboard-vite-dev' // the registry's key for this server — `ports.py list` shows it
const OWNER = 'tauri-dashboard'

const SELF = fileURLToPath(import.meta.url)
const REPO_ROOT = join(dirname(SELF), '..')
const PORTS_SKILL = join(homedir(), '.claude', 'skills', 'ports')
const SHARED = join(PORTS_SKILL, 'scripts', 'dev-port.mjs')

// The host string Vite will bind, so a probe asks about the socket Vite actually takes. `localhost`
// resolves to ::1 on some machines and 127.0.0.1 on others, and the two are separately bindable.
const DEV_HOST = process.env.TAURI_DEV_HOST || 'localhost'

// Set by the resolving pass and read by the launching pass, so the run can name its own source. Without
// it a registry-resolved port is indistinguishable from one a stray PORT supplied, since both arrive as
// PORT in the environment.
const SOURCE_VAR = 'CCDASH_DEV_PORT_SOURCE'

// npm runs scripts at the package root, but a hand-run from elsewhere would make the re-entry path below
// resolve against the wrong directory.
process.chdir(REPO_ROOT)

const args = process.argv.slice(2)

function fail(message) {
  console.error(`dev.mjs: ${message}`)
  process.exit(1)
}

/** How to set PORT in the shell the reader is actually using. */
function portHint() {
  if (process.platform !== 'win32') return 'PORT=<n> npm run tauri dev'
  return 'PowerShell: $env:PORT="<n>"; npm run tauri dev    cmd.exe: set PORT=<n>  then  npm run tauri dev'
}

/** The Tauri CLI's own JS entry point, so it is run by node with no shell between. */
function tauriEntry() {
  const pkgPath = join(REPO_ROOT, 'node_modules', '@tauri-apps', 'cli', 'package.json')
  let bin
  try {
    bin = JSON.parse(readFileSync(pkgPath, 'utf8')).bin
  } catch (error) {
    fail(`could not read ${pkgPath} (${error.message}). Run \`npm install\` first.`)
  }
  const entry = typeof bin === 'string' ? bin : bin && bin.tauri
  if (!entry) fail(`@tauri-apps/cli declares no \`tauri\` bin in ${pkgPath}. Run \`npm install\` first.`)
  return join(REPO_ROOT, 'node_modules', '@tauri-apps', 'cli', entry)
}

/**
 * Run the Tauri CLI with this process's arguments, then exit with its status.
 *
 * Spawned as `node <cli entry>` rather than through the `.bin` shim: the shim is a `.cmd` file on
 * Windows, which CreateProcess will not run, and the usual answer — `shell: true` — makes node
 * concatenate the arguments unescaped (DEP0190), so a clone under a path containing a space fails with
 * an error naming half the path. Resolving the entry means no shell on either platform.
 */
function launch(extraArgs, removeAfter) {
  const child = spawnSync(process.execPath, [tauriEntry(), ...args, ...extraArgs], { stdio: 'inherit' })
  // spawnSync returns only once the CLI has exited, so the override file is no longer being watched and
  // removing it here leaves none behind — one per run would otherwise accumulate in the temp directory.
  if (removeAfter) rmSync(removeAfter, { force: true })
  if (child.error) fail(`could not run the Tauri CLI: ${child.error.message}`)
  process.exit(child.status === null ? 1 : child.status)
}

if (args[0] !== 'dev') launch([])

/** A port number, or null when `raw` is not one. */
function asPort(raw) {
  const text = String(raw).trim()
  if (!/^\d{1,5}$/.test(text)) return null
  const port = Number(text)
  return port >= 1 && port <= 65535 ? port : null
}

if (process.env.PORT) {
  const port = asPort(process.env.PORT)
  if (port === null) {
    fail(`PORT is set to ${JSON.stringify(process.env.PORT)}, which is not a port number. `
       + `Unset it, or set it to one: ${portHint()}`)
  }
  // Normalized, so the child sees the number this run announced. `set PORT=1420 && …` in cmd.exe keeps
  // the trailing space, and Vite's own Number() tolerating it is incidental rather than promised.
  process.env.PORT = String(port)

  const source = process.env[SOURCE_VAR] || 'PORT in the environment, which outranks the ports registry'
  console.error(`dev.mjs: dev server at http://${DEV_HOST}:${port} — port from ${source}`)

  // Per-pid, because two concurrent dev runs on different ports sharing one filename let the later
  // writer repoint the earlier run's webview at the wrong server, with nothing reporting it.
  const overridePath = join(tmpdir(), `${OWNER}-dev-config-${process.pid}.json`)
  const override = { build: { devUrl: `http://${DEV_HOST}:${port}` } }
  try {
    writeFileSync(overridePath, JSON.stringify(override))
  } catch (error) {
    fail(`could not write the Tauri config override to ${overridePath} (${error.message}).`)
  }
  launch(['--config', overridePath], overridePath)
}

/**
 * Whether this machine has the ports registry installed at all.
 *
 * Three answers, not two. `existsSync` collapses "nothing is there" with "something is there and I
 * could not look", and the second must never reach the fallback: on the author's machine
 * `~/.claude/skills` is a symlink into the dotfiles checkout, so moving or renaming that checkout leaves
 * a dangling link that `existsSync` reports exactly as a contributor's bare machine — and the run would
 * then bind an unallocated port while the registry still records one for this project.
 */
function registryPresence() {
  let linked = true
  try {
    lstatSync(PORTS_SKILL)
  } catch (error) {
    if (error.code === 'ENOENT') linked = false
    else return { state: 'damaged', why: `${PORTS_SKILL} could not be read (${error.code})` }
  }
  try {
    statSync(PORTS_SKILL)
    return { state: 'present' }
  } catch (error) {
    if (error.code !== 'ENOENT') return { state: 'damaged', why: `${PORTS_SKILL} could not be read (${error.code})` }
    if (linked) return { state: 'damaged', why: `${PORTS_SKILL} is a link that resolves to nothing` }
    return { state: 'absent' }
  }
}

/** A port the OS says is free on the host Vite will bind. */
async function portFromKernel() {
  return new Promise((resolve, reject) => {
    const probe = createServer()
    probe.once('error', reject)
    probe.listen(0, DEV_HOST, () => {
      const { port } = probe.address()
      probe.close(err => (err ? reject(err) : resolve(port)))
    })
  })
}

const presence = registryPresence()

if (presence.state === 'damaged') {
  fail(`${presence.why}.\n`
     + `A damaged ports registry is not a missing one, so this is NOT falling back to a kernel-assigned `
     + `port — a port nothing allocated would collide with whatever the registry has recorded for this `
     + `project. Repair the dotfiles checkout, or set the port yourself: ${portHint()}`)
}

if (presence.state === 'absent') {
  let port
  try {
    port = await portFromKernel()
  } catch (error) {
    fail(`no ports registry is installed on this machine, and the OS would not name a free port on `
       + `${DEV_HOST} either (${error.message}). Set one yourself: ${portHint()}`)
  }
  console.error(
    `dev.mjs: no ports registry on this machine (${PORTS_SKILL} is not there), so the OS chose a free `
    + `port for this run: ${port}. It changes every run. The port is claimed about a second after it is `
    + `picked, so if something takes it in between Vite stops with "Port ${port} is already in use" `
    + `rather than moving elsewhere — re-run and a different number is chosen. To fix the number: `
    + `${portHint()}`,
  )
  process.env[SOURCE_VAR] = 'the OS, because no ports registry is installed'
  process.env.PORT = String(port)
  const child = spawnSync(process.execPath, [SELF, ...args], { stdio: 'inherit', env: process.env })
  if (child.error) fail(`could not re-enter ${SELF}: ${child.error.message}`)
  process.exit(child.status === null ? 1 : child.status)
}

let run
try {
  ;({ run } = await import(pathToFileURL(SHARED).href))
} catch (error) {
  fail(`the ports registry is installed at ${PORTS_SKILL} but ${SHARED} would not load `
     + `(${error.message}).\nThis is a damaged checkout rather than a missing one, so no fallback is `
     + `taken. Repair it, or set the port yourself: ${portHint()}`)
}

// Set before the call rather than passed to it: run() takes no env option, and spawns the command with
// `{...process.env, PORT}`, so this is how the sentinel reaches the launching pass. Without it that pass
// cannot tell the registry's own answer from a PORT someone exported for another project.
process.env[SOURCE_VAR] = 'the ports registry'

// Relative rather than absolute, so the command carries no path that could contain a space: run()
// routes through a shell on Windows, and whether it quotes for one depends on which version of the
// shared library a given machine has installed. The chdir above makes this resolve the same however
// this script was invoked.
await run({ useCase: USE_CASE, owner: OWNER, command: ['node', join('scripts', 'dev.mjs'), ...args] })
