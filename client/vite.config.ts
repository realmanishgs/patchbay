import { defineConfig } from "vite";
import tailwindcss from "@tailwindcss/vite";

// Tauri expects a fixed dev port and a dist folder that src-tauri/tauri.conf.json points at.
export default defineConfig({
  plugins: [tailwindcss()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    // Cargo writes build artifacts under src-tauri/target while `tauri dev` compiles;
    // watching them causes EBUSY locks on Windows.
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
  },
});
