import { defineConfig, type Plugin } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

const host = process.env.TAURI_DEV_HOST

// The dev server's port is settled at launch by scripts/dev.mjs — from PORT, else the ports registry,
// else the OS — and handed down as PORT. Naming a number here would be a second copy of that value, and
// the copy the server actually binds.
function devPort(): number | null {
  const port = Number(process.env.PORT)
  return Number.isInteger(port) && port >= 1 && port <= 65535 ? port : null
}

// Vite reads no PORT of its own, so an unset one would leave the server on Vite's 5173 default at an
// address nothing else was told. Refuse when a server is created rather than when this file is loaded:
// `npm run check` loads it too, and a throw there fails the config load while the check still reports
// success.
const requireResolvedPort: Plugin = {
  name: 'require-resolved-port',
  configureServer() {
    if (devPort() === null) {
      throw new Error(
        'PORT is unset or is not a port number. Start the dev server with `npm run tauri dev`, which runs '
        + 'scripts/dev.mjs to settle the port and pass it to both halves of the dev run — from the ports '
        + 'registry where one is installed, otherwise from the OS. Or set PORT yourself.',
      )
    }
  },
}

export default defineConfig(async () => ({
  plugins: [svelte(), requireResolvedPort],
  clearScreen: false,
  server: {
    port: devPort() ?? undefined,
    strictPort: true,
    host: host || false,
    watch: { ignored: ['**/src-tauri/**'] },
  },
}))
