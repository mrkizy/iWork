// 行登记表的网页一侧：RowView 的每一种变体一个组件（加一种行 = 这里一臂）。
import type { JSX } from "solid-js";
import type { RowView as Row } from "../gen/iwork/v1/view_pb";
import { ApprovalCard } from "./ApprovalCard";
import { MessageRow } from "./MessageRow";
import { ResumeCard } from "./ResumeCard";
import { ToolRow } from "./ToolRow";
import { TurnEndRow } from "./TurnEndRow";
import { UserActionCard } from "./UserActionCard";
import "./rows.css";

export function RowView(props: { row: Row }): JSX.Element {
  // 函数子节点：行对象整个换掉时重新分派；流式追加改的是同一个对象里的正文，只更新正文。
  return <>{(() => renderRow(props.row.row))()}</>;
}

function renderRow(r: Row["row"]): JSX.Element {
  switch (r.case) {
    case "message":
      return <MessageRow row={r.value} />;
    case "tool":
      return <ToolRow row={r.value} />;
    case "note":
      // 切到全开的那一行用赭红（ui-m1 第 2 节第 9 条），按 Rust 给的字段，不看文字
      return <div class={`iw-row iw-note${r.value.fullAccess ? " iw-note--full" : ""}`}>{r.value.text}</div>;
    case "modelSwitch":
      return <div class="iw-row iw-note">{r.value.text}</div>;
    case "divider":
      return <div class="iw-row iw-divider">{r.value.text}</div>;
    case "turnEnd":
      return <TurnEndRow row={r.value} />;
    case "approval":
      return <ApprovalCard row={r.value} />;
    case "userAction":
      return <UserActionCard row={r.value} />;
    case "resume":
      return <ResumeCard row={r.value} />;
    case undefined:
      return null;
  }
}
