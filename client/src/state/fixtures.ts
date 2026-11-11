// 录像文件（crates/iwork-client-core/examples/dump-view-fixtures.rs 写出）：长度前缀拼接的 ViewPatch。
// 预览页与测试共用这一个解码函数。
import { fromBinary } from "@bufbuild/protobuf";
import { sizeDelimitedPeek } from "@bufbuild/protobuf/wire";
import { ViewPatchSchema, type ViewPatch } from "../gen/iwork/v1/view_pb";

export interface FixtureEntry {
  name: string;
  about: string;
  /** 录像打开的对话；没有就是首页。 */
  focus: string | null;
}

export function decodeFrames(bytes: Uint8Array): ViewPatch[] {
  const out: ViewPatch[] = [];
  let rest = bytes;
  while (rest.length > 0) {
    const peek = sizeDelimitedPeek(rest);
    if (peek.eof || peek.offset + peek.size > rest.length) {
      throw new Error(`录像在第 ${out.length + 1} 帧处截断（剩 ${rest.length} 字节）`);
    }
    out.push(fromBinary(ViewPatchSchema, rest.subarray(peek.offset, peek.offset + peek.size)));
    rest = rest.subarray(peek.offset + peek.size);
  }
  return out;
}
