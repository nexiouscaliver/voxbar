import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { resolve } from "path";
import pkg from "./package.json";

const host = process.env.TAURI_DEV_HOST;

// https://vitejs.dev/config/
export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],

  // KB-096: inject the real package version at build time so the footer
  // and About fallback can never drift from the shipping version (the
  // hardcoded "0.1.2" predates v1.0 and kept showing in tests and plain
  // browser previews). appVersion.ts reads it via typeof so non-vite
  // contexts (bun tests) fall back to the synced literal instead of a
  // ReferenceError.
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },

  // Path aliases
  resolve: {
    alias: {
      "@": resolve(__dirname, "./src"),
      "@/bindings": resolve(__dirname, "./src/bindings.ts"),
    },
  },

  // Multiple entry points for main app and overlay
  build: {
    // Local Tauri app: bundles load from disk, so chunk size is not a
    // delivery concern; 2000 kB sits above the largest chunk (the
    // noticeMessage translation bundle) and silences the 500 kB nag.
    chunkSizeWarningLimit: 2000,
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
        overlay: resolve(__dirname, "src/overlay/index.html"),
      },
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
