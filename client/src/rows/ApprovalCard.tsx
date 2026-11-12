// 审批卡片（ui-m1.md 第 2 节第 8 条，画布 10-ApprovalMode-*）：标题与标签、命令、说明、AI 审查一行；待定时
// 「允许」「记住：… ▾」「拒绝」。记住下拉只列 Rust 给的 remember_options（必问、清单外沙箱外执行时为空，就不出下拉）。
// 切档放行后折叠成一行；已作废置灰。按钮可否点击、状态文字都来自 patch。安全类卡片，不接任何轻松元素（3.4）。
import { createSignal, For, Show } from "solid-js";
import { RememberScope } from "../gen/iwork/v1/common_pb";
import type { ApprovalRow } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Tag } from "../ui/Badge";
import { Button } from "../ui/Button";
import { Icon } from "../ui/Icon";
import { anchorBelow, Menu, MenuItem, type MenuAnchor } from "../ui/Menu";
import "./cards.css";

export function ApprovalCard(props: { row: ApprovalRow }) {
  const r = () => props.row;
  return (
    <Show when={!r().folded} fallback={<FoldedApproval row={r()} />}>
      <div class="iw-row">
        <section class={`iw-card iw-card--approval${r().muted ? " iw-card--muted" : ""}`} aria-label={r().title}>
          <header class="iw-card__head">
            <span class="iw-card__title">{r().title}</span>
            <For each={r().tags}>{(t) => <Tag tone="warn">{t}</Tag>}</For>
            <Show when={r().stateLabel}>
              <span class="iw-card__state">{r().stateLabel}</span>
            </Show>
          </header>
          <pre class="iw-card__code">{r().code}</pre>
          <For each={r().notes}>{(n) => <p class="iw-card__note">{n}</p>}</For>
          <Show when={r().reviewLine}>
            <p class="iw-card__review">{r().reviewLine}</p>
          </Show>
          {/* 待定（没有状态文字）才有按钮；断线时按钮还在、不可点 */}
          <Show when={!r().stateLabel}>
            <Decide row={r()} />
          </Show>
        </section>
      </div>
    </Show>
  );
}

function Decide(props: { row: ApprovalRow }) {
  const host = useHost();
  // 记住的范围是网页本地的选择（菜单开合同类）。默认「仅这一次」，列表里没有它时取第一项。
  const options = () => props.row.rememberOptions;
  const once = () => Math.max(0, options().findIndex((o) => o.value === RememberScope.ONCE));
  const [picked, setRemember] = createSignal<number>();
  const remember = () => picked() ?? once();
  const [menu, setMenu] = createSignal<MenuAnchor>();
  const chosen = () => options()[remember()];
  const decide = (allow: boolean) =>
    void host.act({
      case: "decideApproval",
      value: { approvalId: props.row.approvalId, allow, remember: allow ? (chosen()?.value ?? 0) : 0 },
    });
  return (
    <div class="iw-card__actions">
      <Button variant="primary" disabled={!props.row.canDecide} onClick={() => decide(true)}>
        允许
      </Button>
      <Show when={options().length > 0}>
        <Button
          class="iw-card__remember"
          aria-haspopup="menu"
          aria-expanded={menu() !== undefined}
          disabled={!props.row.canDecide}
          onClick={(e) => setMenu(anchorBelow(e.currentTarget))}
        >
          记住：{chosen()?.label}
          <Icon name="chevronDown" size={12} />
        </Button>
      </Show>
      <Button class="iw-btn--deny" disabled={!props.row.canDecide} onClick={() => decide(false)}>
        拒绝
      </Button>
      <Show when={menu()}>
        {(at) => (
          <Menu anchor={at()} label="记住的范围" onClose={() => setMenu(undefined)}>
            <For each={options()}>
              {(o, i) => (
                <MenuItem
                  selected={i() === remember()}
                  onSelect={() => {
                    setRemember(i());
                    setMenu(undefined);
                  }}
                >
                  {o.label}
                </MenuItem>
              )}
            </For>
          </Menu>
        )}
      </Show>
    </div>
  );
}

/** 切档放行后的折叠形态（画布 10-ApprovalMode-auto）：一行命令 + 状态标签。 */
function FoldedApproval(props: { row: ApprovalRow }) {
  return (
    <div class="iw-row iw-approval-folded">
      <span class="iw-approval-folded__code">{props.row.code}</span>
      <Tag>{props.row.stateLabel}</Tag>
    </div>
  );
}
