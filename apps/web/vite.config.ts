import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Web dev server：把 /api 与 /ws 代理到本地 TPI Server（`tpi server`）。
// 生产构建直接由 tpi-server 提供静态资源，无需代理。
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": "http://127.0.0.1:8765",
      "/ws": {
        target: "ws://127.0.0.1:8765",
        ws: true,
      },
    },
  },
  build: {
    outDir: "dist",
    sourcemap: false,
  },
});
