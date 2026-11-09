/* @refresh reload */
import { render } from "solid-js/web";

// 空壳：只证明 SolidJS 渲染链路能跑通。界面由 T09 起填。
const root = document.getElementById("root");
if (root === null) {
  throw new Error("index.html 缺少 #root 挂载点");
}
render(() => <main />, root);
