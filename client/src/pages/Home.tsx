// 首页（画布 01）：「<项目名> / 要做点什么？」与输入框。还没有对话：发出第一条消息时先新建对话，建好后跳过去再发（shell/starter）。
import { Show } from "solid-js";
import { Composer } from "../composer/Composer";
import { HOME_DRAFT } from "../composer/drafts";
import { useHost } from "../host";
import { useStarter } from "../shell/starter";
import "./pages.css";

export function HomePage(props: { projectId?: string }) {
  const host = useHost();
  const starter = useStarter();
  const project = () => {
    const ps = host.view.sidebar?.projects ?? [];
    return ps.find((p) => p.projectId === props.projectId) ?? ps[0];
  };
  return (
    <div class="iw-conv-page">
      <div class="iw-hero">
        <Show when={project()}>{(p) => <div class="iw-hero__project">{p().name}</div>}</Show>
        <h1 class="iw-hero__title">要做点什么？</h1>
      </div>
      <Show when={project()} fallback={<div class="iw-blocking">先添加一个项目</div>}>
        {(p) => (
          <Composer
            draftKey={HOME_DRAFT + p().projectId}
            starting={starter.pending() !== undefined}
            onStart={(text) => void starter.start(p().projectId, text)}
          />
        )}
      </Show>
    </div>
  );
}
