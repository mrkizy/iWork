// 默认配置的 beforeBuildCommand 指向这里：有人绕过包装脚本裸跑 `tauri build` 时，
// 没有构建层就没有前端资源，Tauri 照样会打出一个空壳安装包，所以在这一步明确失败。
// 走 `pnpm tauri build` 时构建层（tauri.build.conf.json）会把 beforeBuildCommand 换成真正的前端构建。
console.error(
  "拒绝构建：没有叠加构建配置层（src-tauri/tauri.build.conf.json），产物会缺前端资源。\n" +
    "请在 client/ 下用 `pnpm tauri build`，不要直接调用 tauri CLI。",
);
process.exit(1);
