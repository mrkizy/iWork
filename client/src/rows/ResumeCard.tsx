// 继续 / 放弃卡片：回合中断后（后台崩溃、等审批太久），由你决定接着跑还是放弃。处理完卡片消失、不留痕（Q10，
// Rust 侧随待处理表移除）。原因文字与可否点击来自 patch。
import type { ResumeRow } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Button } from "../ui/Button";
import { Icon } from "../ui/Icon";
import "./cards.css";

export function ResumeCard(props: { row: ResumeRow }) {
  const host = useHost();
  const turn = (k: "resumeTurn" | "abandonTurn") => void host.act({ case: k, value: { turnId: props.row.turnId } });
  return (
    <div class="iw-row">
      <section class="iw-card iw-card--resume" aria-label="回合中断了">
        <header class="iw-card__head">
          <Icon name="alert" size={14} />
          <span class="iw-card__title">{props.row.reason}</span>
        </header>
        <div class="iw-card__actions">
          <Button variant="primary" disabled={!props.row.canResume} onClick={() => turn("resumeTurn")}>
            继续
          </Button>
          <Button disabled={!props.row.canAbandon} onClick={() => turn("abandonTurn")}>
            放弃
          </Button>
        </div>
      </section>
    </div>
  );
}
