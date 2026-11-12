// 预览专用：设计系统里全部形象 × 全部表情一览（UI 审查第 5 轮建议；正式应用里没有这一页）。
// 地址 /preview.html#/characters?theme=light|dark。素材是设计系统自带的静态文件，不是协议内容，所以直接读清单，
// 不经录像：表情大多由 Rust 随真实状态给，靠假后台事件凑不齐 13 × 15 种。用和应用里同一个 Avatar 组件画。
// 某个形象缺哪种表情就标「缺」，不让 Avatar 的兜底规则悄悄画成 idle。
import { create } from "@bufbuild/protobuf";
import { For, Show } from "solid-js";
import { AvatarViewSchema } from "../gen/iwork/v1/view_pb";
import { Avatar } from "../ui/Avatar";
import type { CharacterCatalog } from "../ui/characters";

const SIZE = 56;

export function CharacterGallery(props: { catalog: CharacterCatalog }) {
  const characters = () => [...props.catalog.characters.keys()].sort();
  const moods = () => [...new Set([...props.catalog.characters.values()].flatMap((m) => [...m.keys()]))].sort();
  return (
    <main class="iw-preview-index iw-preview-gallery">
      <h1>形象一览</h1>
      <p>
        {characters().length} 个形象 × {moods().length} 种表情（设计系统 characters/ 目录）。测试专用：M1 没有选形象的入口。
      </p>
      <table>
        <thead>
          <tr>
            <th />
            <For each={moods()}>{(m) => <th scope="col">{m}</th>}</For>
          </tr>
        </thead>
        <tbody>
          <For each={characters()}>
            {(c) => (
              <tr data-character={c}>
                <th scope="row">{c}</th>
                <For each={moods()}>
                  {(m) => (
                    <td data-mood={m}>
                      <Show when={props.catalog.characters.get(c)?.has(m)} fallback={<span class="iw-preview-gallery__missing">缺</span>}>
                        <Avatar avatar={create(AvatarViewSchema, { character: c, mood: m })} size={SIZE} label={`${c} ${m}`} />
                      </Show>
                    </td>
                  )}
                </For>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </main>
  );
}
