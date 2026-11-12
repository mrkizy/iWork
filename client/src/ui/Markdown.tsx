// Markdown 正文（方案 2.5、R7）：模型输出、工具输出都是不可信输入。
// 解析成语法树（mdast）后用组件逐个节点渲染，不经 HTML 字符串、不用 innerHTML；原始 HTML 当文字显示；
// 链接不设 href（不会被点、拖、中键导航），点击只交给调用方发 OpenExternal，且只放行 http / https；
// 远程图片不加载，显示成链接文字。
import type { Nodes, Parents, RootContent } from "mdast";
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfmFromMarkdown } from "mdast-util-gfm";
import { gfm } from "micromark-extension-gfm";
import { createMemo, For, Match, Switch, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";
import "./Markdown.css";

export function parseMarkdown(text: string) {
  return fromMarkdown(text, { extensions: [gfm()], mdastExtensions: [gfmFromMarkdown()] });
}

/** 只有 http / https 链接可以交给系统浏览器打开。 */
export function isExternalUrl(url: string): boolean {
  try {
    const u = new URL(url);
    return u.protocol === "http:" || u.protocol === "https:";
  } catch {
    return false;
  }
}

type OpenLink = (url: string) => void;

export function Markdown(props: { text: string; onOpenLink: OpenLink; class?: string }) {
  const tree = createMemo(() => parseMarkdown(props.text));
  return (
    <div class={`iw-md${props.class ? ` ${props.class}` : ""}`}>
      <Children node={tree()} open={props.onOpenLink} />
    </div>
  );
}

function Children(props: { node: Parents; open: OpenLink }) {
  return <For each={props.node.children as RootContent[]}>{(child) => <Node node={child} open={props.open} />}</For>;
}

function Link(props: { url: string; open: OpenLink; children: JSX.Element }) {
  const external = () => isExternalUrl(props.url);
  return (
    <span
      class={external() ? "iw-md__link" : undefined}
      role={external() ? "link" : undefined}
      tabindex={external() ? 0 : undefined}
      title={props.url}
      onClick={() => external() && props.open(props.url)}
      onKeyDown={(e) => e.key === "Enter" && external() && props.open(props.url)}
    >
      {props.children}
    </span>
  );
}

const HEADINGS = ["h1", "h2", "h3", "h4", "h5", "h6"] as const;

function Node(props: { node: Nodes; open: OpenLink }): JSX.Element {
  const n = props.node;
  // 一个节点种类一臂；没列到的节点按它的纯文字显示，不丢内容。
  switch (n.type) {
    case "paragraph":
      return (
        <p>
          <Children node={n} open={props.open} />
        </p>
      );
    case "heading":
      return (
        <Dynamic component={HEADINGS[n.depth - 1]}>
          <Children node={n} open={props.open} />
        </Dynamic>
      );
    case "text":
      return <>{n.value}</>;
    case "emphasis":
      return (
        <em>
          <Children node={n} open={props.open} />
        </em>
      );
    case "strong":
      return (
        <strong>
          <Children node={n} open={props.open} />
        </strong>
      );
    case "delete":
      return (
        <del>
          <Children node={n} open={props.open} />
        </del>
      );
    case "inlineCode":
      return <code>{n.value}</code>;
    case "code":
      return (
        <pre>
          <code>{n.value}</code>
        </pre>
      );
    case "blockquote":
      return (
        <blockquote>
          <Children node={n} open={props.open} />
        </blockquote>
      );
    case "list":
      return (
        <Switch>
          <Match when={n.ordered}>
            <ol start={n.start ?? undefined}>
              <Children node={n} open={props.open} />
            </ol>
          </Match>
          <Match when={!n.ordered}>
            <ul>
              <Children node={n} open={props.open} />
            </ul>
          </Match>
        </Switch>
      );
    case "listItem":
      return (
        <li>
          {n.checked === true ? "☑ " : n.checked === false ? "☐ " : ""}
          <Children node={n} open={props.open} />
        </li>
      );
    case "link":
      return (
        <Link url={n.url} open={props.open}>
          <Children node={n} open={props.open} />
        </Link>
      );
    case "image":
      return (
        <Link url={n.url} open={props.open}>
          {`[图片${n.alt ? `：${n.alt}` : ""}]`}
        </Link>
      );
    case "break":
      return <br />;
    case "thematicBreak":
      return <hr />;
    case "html":
      // 原始 HTML 当文字显示
      return <>{n.value}</>;
    case "table":
      return (
        <table>
          <tbody>
            <For each={n.children}>
              {(row) => (
                <tr>
                  <For each={row.children}>
                    {(cell) => (
                      <td>
                        <Children node={cell} open={props.open} />
                      </td>
                    )}
                  </For>
                </tr>
              )}
            </For>
          </tbody>
        </table>
      );
    default:
      return <>{plainText(n)}</>;
  }
}

function plainText(n: Nodes): string {
  if ("value" in n && typeof n.value === "string") return n.value;
  if ("children" in n) return (n.children as Nodes[]).map(plainText).join("");
  return "";
}
