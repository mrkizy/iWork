// 设置页（画布 12 的左栏；方案 2.7）：整页，左边「‹ 返回」与几节，右边是那一节。
// 通用（轻松元素、登录自启）、项目位置（「更换」由 Rust 弹文件夹选择框，网页不碰路径）、运行时（状态与验证标签
// 只在这里出现，R10；未安装、未登录给文字指引与「重新检测」；M2 才接入的置灰）、审批（记住的规则：分组、撤销）、
// 关于。值、文字、可否点击都来自 SettingsView；点了只发操作，等下一帧（方案 2.5）。
// 本机导入的列表等 architect 与设计窗口定形态（组 3 B4），这次不画「从本机导入」入口。
import { For, Match, Show, Switch as Choose } from "solid-js";
import type { RuntimeView, SettingsView } from "../../gen/iwork/v1/view_pb";
import { useHost } from "../../host";
import type { SettingsSection } from "../../shell/route";
import { Spinner } from "../../ui/Badge";
import { Button, IconButton } from "../../ui/Button";
import { Switch } from "../../ui/Switch";
import iconUrl from "@design/icon/iwork-icon-white-256.png";
import "./settings.css";

const SECTIONS: { id: SettingsSection; label: string }[] = [
  { id: "general", label: "通用" },
  { id: "projectsRoot", label: "项目位置" },
  { id: "runtimes", label: "运行时" },
  { id: "approval", label: "审批" },
  { id: "about", label: "关于" },
];

export function SettingsPage(props: { section?: SettingsSection; go: (s: SettingsSection) => void; back: () => void }) {
  const host = useHost();
  const section = () => props.section ?? "general";
  return (
    <div class="iw-settings">
      <nav class="iw-settings__nav" aria-label="设置">
        <button type="button" class="iw-settings__back" onClick={() => props.back()}>
          ‹ 返回
        </button>
        <h1 class="iw-settings__title">设置</h1>
        <For each={SECTIONS}>
          {(s) => (
            <button
              type="button"
              class="iw-settings__item"
              aria-current={section() === s.id ? "page" : undefined}
              onClick={() => props.go(s.id)}
            >
              {s.label}
            </button>
          )}
        </For>
      </nav>
      <main class="iw-settings__body">
        <h2 class="iw-settings__heading">{SECTIONS.find((s) => s.id === section())?.label}</h2>
        <Show when={host.view.settings} fallback={<Spinner label="正在读取设置" />}>
          {(v) => (
            <Choose>
              <Match when={section() === "general"}>
                <General view={v()} />
              </Match>
              <Match when={section() === "projectsRoot"}>
                <ProjectsRoot view={v()} />
              </Match>
              <Match when={section() === "runtimes"}>
                <For each={v().runtimes}>{(r) => <Runtime r={r} />}</For>
              </Match>
              <Match when={section() === "approval"}>
                <Rules view={v()} />
              </Match>
              <Match when={section() === "about"}>
                <div class="iw-settings__about">
                  <img src={iconUrl} alt="" width="48" height="48" />
                  <span>iWork</span>
                </div>
              </Match>
            </Choose>
          )}
        </Show>
      </main>
    </div>
  );
}

function General(props: { view: SettingsView }) {
  const host = useHost();
  return (
    <div class="iw-settings__card">
      <div class="iw-settings__row">
        <span>轻松元素</span>
        <Switch
          label="轻松元素"
          checked={props.view.playfulUi}
          onChange={(on) => void host.act({ case: "setPlayfulUi", value: { on } })}
        />
      </div>
      <div class="iw-settings__row">
        <span>登录时启动</span>
        <Switch
          label="登录时启动"
          checked={props.view.autostart}
          onChange={(on) => void host.act({ case: "setAutostart", value: { on } })}
        />
      </div>
    </div>
  );
}

function ProjectsRoot(props: { view: SettingsView }) {
  const host = useHost();
  return (
    <div class="iw-settings__card iw-settings__row">
      <span class="iw-settings__path">{props.view.projectsRoot}</span>
      <Button onClick={() => void host.act({ case: "pickProjectsRoot", value: {} })}>更换</Button>
    </div>
  );
}

function Runtime(props: { r: RuntimeView }) {
  const host = useHost();
  const r = () => props.r;
  const line = () => [r().stateLabel, r().loginLabel, r().versionLabel, r().verificationLabel].filter(Boolean).join(" · ");
  return (
    <section class={`iw-settings__card${r().upcoming ? " iw-settings__card--upcoming" : ""}`} aria-label={r().displayName}>
      <div class="iw-settings__row">
        <div>
          <div class="iw-settings__name">{r().displayName}</div>
          <div class="iw-settings__meta">{r().upcoming ? r().stateLabel : line()}</div>
          <Show when={r().guidance}>{(g) => <p class="iw-settings__guidance">{g()}</p>}</Show>
        </div>
        <Show when={r().canRedetect}>
          <Button onClick={() => void host.act({ case: "redetectRuntime", value: { runtimeId: r().runtimeId } })}>
            重新检测
          </Button>
        </Show>
      </div>
    </section>
  );
}

function Rules(props: { view: SettingsView }) {
  const host = useHost();
  return (
    <Show when={!props.view.rulesLoading} fallback={<Spinner label="正在读取记住的规则" />}>
      <Show when={props.view.ruleGroups.length > 0} fallback={<p class="iw-settings__empty">{props.view.rulesEmptyText}</p>}>
        <For each={props.view.ruleGroups}>
          {(g) => (
            <section class="iw-settings__group" aria-label={g.title}>
              <h3 class="iw-settings__group-title">{g.title}</h3>
              <ul class="iw-settings__card iw-settings__rules">
                <For each={g.rules}>
                  {(rule) => (
                    <li class="iw-settings__row">
                      <span>{rule.label}</span>
                      <Show when={rule.canRevoke}>
                        <IconButton
                          icon="close"
                          label="撤销"
                          onClick={() => void host.act({ case: "revokeRule", value: { ruleId: rule.ruleId } })}
                        />
                      </Show>
                    </li>
                  )}
                </For>
              </ul>
            </section>
          )}
        </For>
      </Show>
    </Show>
  );
}
