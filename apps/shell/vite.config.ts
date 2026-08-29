import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri v2 dev server pairing: fixed port, no auto-fallback (tauri.conf.json
// devUrl points at 5173), and don't watch the Rust side.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    target: "es2021",
    outDir: "dist",
    emptyOutDir: true,
  },
});
