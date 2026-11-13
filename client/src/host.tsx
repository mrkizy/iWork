// 网页宿主：组件从这里读视图状态、发操作。正式入口接 Tauri（index.tsx），录像播放页接录像（preview/）。
// 组件只读 view、只调 act；点了按钮不在本地改状态，等下一帧（方案 2.5）。
import { createContext, useContext } from "solid-js";
import type { IntentKind } from "./bridge/intent";
import type { IntentAck } from "./gen/iwork/v1/view_pb";
import type { ViewState } from "./state/store";

export interface Host {
  readonly view: ViewState;
  /** 发一个操作；被拒时（IntentAck 带错误）由宿主显示提示，调用方也能拿到受理结果。
   *  `quiet`：调用方自己把错误显示在原处（如表单弹窗），宿主不再弹提示。 */
  act(kind: IntentKind, opts?: { quiet?: boolean }): Promise<IntentAck>;
}

const HostContext = createContext<Host>();

export const HostProvider = HostContext.Provider;

export function useHost(): Host {
  const host = useContext(HostContext);
  if (host === undefined) throw new Error("组件不在 HostProvider 里");
  return host;
}
