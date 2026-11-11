// 首页（画布 01）：「<项目名> / 要做点什么？」与输入框。对着哪个项目由 Rust 定（HomeView），
// 发出首条消息后由 Rust 新建对话、设上首页选的模型与档位、发出，再请网页跳过去（client-core session::start）。
import { Show } from "solid-js";
import { Composer } from "../composer/Composer";
import { HOME_DRAFT } from "../composer/drafts";
import { useHost } from "../host";
import "./pages.css";

export function HomePage() {
  const host = useHost();
  const home = () => host.view.home;
  return (
    <div class="iw-conv-page">
      <div class="iw-hero">
        <Show when={home()?.projectName}>{(name) => <div class="iw-hero__project">{name()}</div>}</Show>
        <h1 class="iw-hero__title">要做点什么？</h1>
      </div>
      <Show when={home()?.projectId} fallback={<Show when={home()}><div class="iw-blocking">先添加一个项目</div></Show>}>
        {(id) => (
          <Composer
            target={{ kind: "home", projectId: id() }}
            composer={home()?.composer}
            draftKey={HOME_DRAFT + id()}
            starting={home()?.starting}
            startError={home()?.startError}
          />
        )}
      </Show>
    </div>
  );
}
