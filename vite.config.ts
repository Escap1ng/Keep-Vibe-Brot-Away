import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Tauri 开发服务器端口固定,便于 tauri.conf.json 引用
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    hmr: true,
  },
  build: {
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
  },
});
