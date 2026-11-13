// forged_add_project_70_is_rejected（T09 方案 15.2 SD5）：网页伪造已删掉的旧 70 号意图（带路径的 add_project），
// 在真 WebView 里经真的 ui_intent 命令发给 Rust：Rust 认不出这个操作、回 invalid_intent，什么都不做。
// 对照：同一条路发一个合法的 clear_add_project（79 号）被受理，证明字节确实送到了、回包也读对了。
// 字节手写（不用生成代码）：生成代码里已经没有 70 号，攻击者也不会用它。
window.addEventListener("load", async () => {
  const utf8 = (s) => Array.from(new TextEncoder().encode(s));
  // 旧 AddProjectIntent { path = 1 }，套进 UiIntent 的 70 号字段：标签 (70<<3)|2 = 562 = varint b2 04
  const path = utf8("/etc");
  const inner = [0x0a, path.length, ...path];
  const forged = new Uint8Array([0xb2, 0x04, inner.length, ...inner]);
  // UiIntent.clear_add_project = 79：标签 (79<<3)|2 = 634 = varint fa 04，空消息
  const control = new Uint8Array([0xfa, 0x04, 0x00]);
  // IntentAck { error = 1: IntentError { code = 1 } }：只读出错误码；没有 error 时回包是空的
  const errorCode = (buf) => {
    const b = new Uint8Array(buf);
    if (b.length === 0) return null;
    if (b[0] !== 0x0a || b[2] !== 0x0a) return "unparsed:" + Array.from(b.slice(0, 8)).join(",");
    return new TextDecoder().decode(b.slice(4, 4 + b[3]));
  };
  const send = async (body) => {
    try {
      return { code: errorCode(await window.__TAURI_INTERNALS__.invoke("ui_intent", body)) };
    } catch (e) {
      return { thrown: String(e).slice(0, 120) };
    }
  };
  await __e2e.sleep(1000);
  const forgedReply = await send(forged);
  const controlReply = await send(control);
  await __e2e.sleep(1500);
  __e2e.report({
    case: "forged",
    forged: forgedReply,
    control: controlReply,
    dialogOpen: document.querySelector('[role="dialog"][aria-label="添加项目"]') !== null,
    done: true,
  });
});
