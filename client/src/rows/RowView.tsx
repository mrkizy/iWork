// 行登记表的网页一侧：RowView 的每一种变体一个组件（加一种行 = 这里一臂）。
// 审批、亲手执行、继续 / 放弃卡片与回合结束行的完整样子归步 7b，先画只读的占位，免得时间线缺行。
import type { JSX } from "solid-js";
import type { RowView as Row, TurnEndRow } from "../gen/iwork/v1/view_pb";
import { MessageRow } from "./MessageRow";
import { ToolRow } from "./ToolRow";
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
    case "modelSwitch":
      return <div class="iw-row iw-note">{r.value.text}</div>;
    case "divider":
      return <div class="iw-row iw-divider">{r.value.text}</div>;
    case "turnEnd":
      return <TurnEndLater row={r.value} />;
    case "approval":
      return <Later what="审批卡片" />;
    case "userAction":
      return <Later what="亲手执行卡片" />;
    case "resume":
      return <Later what="继续 / 放弃卡片" />;
    case undefined:
      return null;
  }
}

function Later(props: { what: string }) {
  return (
    <div class="iw-row">
      <div class="iw-row-later">{props.what}（卡片下一组实现）</div>
    </div>
  );
}

/** 回合结束行（步 7b 画完整的摘要与印章）：先只显示 Rust 给的摘要与结果文字，都为空就不占地方。 */
function TurnEndLater(props: { row: TurnEndRow }) {
  const text = () => [props.row.summaryLine, props.row.outcomeLabel].filter(Boolean).join(" · ");
  return <>{text() ? <div class="iw-row iw-note">{text()}</div> : null}</>;
}
