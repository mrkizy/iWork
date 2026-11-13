// 添加项目弹窗（画布 07、07-new；T09 方案 15.2）。两个页签：打开本机文件夹、新建项目。「从 Git 地址克隆」页签等
// 设计窗口答 U-7d-4 再画（方案 15.8），这次不画、不拿推测占位。
// 路径只在 Rust：网页只说「选文件夹」「初始化」「确认」，看到的路径是写成 ~ 的显示文字（RD1）。候选的检查结果、
// 提示框、初始化按钮、AGENTS.md 一行、勾选框、主按钮可不可点与文字，全来自 AddProjectView.candidate。
// 被拒的原因（后台的、或当下不能做的）都在 AddProjectView.error 里，弹窗留着显示它。
// 新建项目的名字是网页给的字符串，后台只当它是一个目录名（方案 15.2「从网页来的字符串」），网页不校验。
// 下面列着待确认的 AGENTS.md 草稿（Q11：添加项目流程与「待你处理」里显示）。
import { createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import { Portal } from "solid-js/web";
import { Tone, type AddProjectView, type CandidateView } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Spinner } from "../ui/Badge";
import { Button, IconButton } from "../ui/Button";
import { rememberFocus } from "../ui/focus";
import { Icon, type IconName } from "../ui/Icon";
import "./addProject.css";

type Tab = "local" | "new";

/** 发命令的几个操作被拒时，原因由 Rust 记进 AddProjectView.error、显示在弹窗里：宿主不再另弹提示。 */
const QUIET = { quiet: true } as const;

const TABS: { id: Tab; icon: IconName; title: string; sub: string }[] = [
  { id: "local", icon: "folder", title: "打开本机文件夹", sub: "选择已有的项目目录" },
  { id: "new", icon: "plus", title: "新建项目", sub: "创建目录并初始化 git" },
];

export function AddProjectDialog(props: { onClose: () => void }) {
  const host = useHost();
  const [tab, setTab] = createSignal<Tab>("local");
  const view = () => host.view.addProject;
  const close = () => {
    void host.act({ case: "clearAddProject", value: {} });
    props.onClose();
  };
  let box!: HTMLDivElement;
  const restoreFocus = rememberFocus();
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  };
  onMount(() => document.addEventListener("keydown", onKey, true));
  onCleanup(() => {
    document.removeEventListener("keydown", onKey, true);
    restoreFocus(box);
  });
  return (
    <Portal>
      <div class="iw-dialog-scrim" onClick={(e) => e.target === e.currentTarget && close()}>
        <div ref={box} class="iw-dialog iw-addproj" role="dialog" aria-modal="true" aria-label="添加项目">
          <header class="iw-addproj__head">
            <h2 class="iw-dialog__title">添加项目</h2>
            <IconButton icon="close" label="关闭" onClick={close} />
          </header>
          <div class="iw-addproj__tabs" role="tablist" aria-label="添加方式">
            <For each={TABS}>
              {(t) => (
                <button
                  type="button"
                  role="tab"
                  class="iw-addproj__tab"
                  aria-selected={tab() === t.id}
                  onClick={() => setTab(t.id)}
                >
                  <Icon name={t.icon} size={16} />
                  <span>
                    <strong>{t.title}</strong>
                    <small>{t.sub}</small>
                  </span>
                </button>
              )}
            </For>
          </div>
          <Show when={view()} fallback={<Spinner />}>
            {(v) => (
              <>
                <Switch>
                  <Match when={tab() === "local"}>
                    <Local view={v()} onCancel={close} />
                  </Match>
                  <Match when={tab() === "new"}>
                    <NewProject view={v()} onCancel={close} />
                  </Match>
                </Switch>
                <Drafts view={v()} />
              </>
            )}
          </Show>
        </div>
      </div>
    </Portal>
  );
}

function Local(props: { view: AddProjectView; onCancel: () => void }) {
  const host = useHost();
  const c = () => props.view.candidate;
  return (
    <div class="iw-addproj__body" role="tabpanel">
      <div class="iw-addproj__pathbox">
        <span class="iw-addproj__path">{c()?.pathLabel ?? "还没有选文件夹"}</span>
        <Button onClick={() => void host.act({ case: "pickProjectFolder", value: {} })}>{c() ? "更换" : "选择"}</Button>
      </div>
      <p class="iw-field__hint">也可以直接把文件夹拖进 iWork 窗口</p>
      <Show when={c()}>{(cand) => <Candidate c={cand()} />}</Show>
      <Show when={props.view.error}>
        {(e) => (
          <p class="iw-addproj__error" role="alert">
            {e()}
          </p>
        )}
      </Show>
      <Show when={props.view.runtimeLabel}>
        <p class="iw-addproj__line">
          <span>运行时</span> {props.view.runtimeLabel}
        </p>
      </Show>
      <footer class="iw-dialog__actions">
        <Button onClick={props.onCancel}>取消</Button>
        <Button
          variant="primary"
          disabled={!c()?.canConfirm || props.view.busy}
          onClick={() => void host.act({ case: "confirmAddProject", value: {} }, QUIET)}
        >
          <Show when={props.view.busy}>
            <Spinner />
          </Show>
          {c()?.confirmLabel ?? "添加并开始对话"}
        </Button>
      </footer>
    </div>
  );
}

function Candidate(props: { c: CandidateView }) {
  const host = useHost();
  const c = () => props.c;
  return (
    <>
      <Show when={c().checking}>
        <Spinner label="正在检查这个文件夹" />
      </Show>
      <Show when={c().noticeTitle}>
        <div class={`iw-addproj__notice${c().noticeTone === Tone.WARNING ? " iw-addproj__notice--warn" : ""}`}>
          <strong>{c().noticeTitle}</strong>
          <Show when={c().noticeBody}>
            <p>{c().noticeBody}</p>
          </Show>
          <Show when={c().initLabel}>
            {(label) => (
              <Button
                disabled={c().initializing}
                onClick={() => void host.act({ case: "initCandidateRepository", value: {} }, QUIET)}
              >
                <Show when={c().initializing}>
                  <Spinner />
                </Show>
                {label()}
              </Button>
            )}
          </Show>
        </div>
      </Show>
      <Show when={c().agentsMdLabel}>
        <p class="iw-addproj__line">
          <span>AGENTS.md</span> {c().agentsMdLabel}
        </p>
      </Show>
      <Show when={c().canProposeDraft}>
        <label class="iw-addproj__check">
          <input
            type="checkbox"
            checked={c().proposeDraft}
            onChange={(e) => void host.act({ case: "setProposeAgentsMdDraft", value: { on: e.currentTarget.checked } })}
          />
          让 iWork 读代码生成 AGENTS.md 草稿，写入前给你确认
        </label>
      </Show>
    </>
  );
}

function NewProject(props: { view: AddProjectView; onCancel: () => void }) {
  const host = useHost();
  const [name, setName] = createSignal("");
  return (
    <div class="iw-addproj__body" role="tabpanel">
      <label class="iw-field">
        <span class="iw-field__label">项目名</span>
        <input class="iw-input" value={name()} onInput={(e) => setName(e.currentTarget.value)} />
      </label>
      <div class="iw-addproj__pathbox iw-addproj__pathbox--plain">
        <span class="iw-addproj__line">
          <span>位置</span> <span class="iw-addproj__path">{[props.view.parentLabel, name()].filter(Boolean).join("/")}</span>
        </span>
        <Button onClick={() => void host.act({ case: "pickProjectsParent", value: {} })}>更换</Button>
      </div>
      <p class="iw-field__hint">默认目录可在 设置 › 项目位置 中修改</p>
      <p class="iw-addproj__line">
        <span>版本控制</span> 创建后自动执行 git init 并首次提交
      </p>
      <Show when={props.view.runtimeLabel}>
        <p class="iw-addproj__line">
          <span>运行时</span> {props.view.runtimeLabel}
        </p>
      </Show>
      <Show when={props.view.error}>
        {(e) => (
          <p class="iw-addproj__error" role="alert">
            {e()}
          </p>
        )}
      </Show>
      <footer class="iw-dialog__actions">
        <Button onClick={props.onCancel}>取消</Button>
        <Button
          variant="primary"
          disabled={props.view.busy}
          onClick={() => void host.act({ case: "createProject", value: { name: name() } }, QUIET)}
        >
          创建并开始对话
        </Button>
      </footer>
    </div>
  );
}

/** 待确认的 AGENTS.md 草稿（Q11）。能不能点跟连接走（断线时命令不可发，D8）。 */
function Drafts(props: { view: AddProjectView }) {
  const host = useHost();
  const live = () => host.view.connection?.commandsEnabled ?? false;
  const decide = (cardId: string, accept: boolean) =>
    void host.act({ case: "decideAgentsMdDraft", value: { cardId, accept } });
  return (
    <Show when={props.view.drafts.length > 0}>
      <section class="iw-addproj__drafts" aria-label="AGENTS.md 草稿">
        <For each={props.view.drafts}>
          {(d) => (
            <div class="iw-addproj__draft">
              <span>{d.projectName} 的 AGENTS.md 草稿</span>
              <Button variant="primary" disabled={!live()} onClick={() => decide(d.cardId, true)}>
                采用
              </Button>
              <Button disabled={!live()} onClick={() => decide(d.cardId, false)}>
                不用
              </Button>
            </div>
          )}
        </For>
      </section>
    </Show>
  );
}
