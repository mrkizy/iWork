// 跨语言金样本测试（T01 方案 3.7、8 节 S3）：Rust 侧（protox + prost）写出的二进制样本，
// 用 TS 侧（buf + protobuf-es）生成的类型解码，转成 proto3 JSON 后与样本旁的期望 JSON 比对。
// 两条生成链路对同一份 .proto 解释不一致时，这里会红。
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createRegistry, fromBinary, toJson, type DescFile } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

// 协议主版本：样本目录名与 proto 包名用同一个数（方案 3.7「身份只有一个」）
const PROTOCOL_MAJOR_DIR = "v1";
const PROTO_PACKAGE = `iwork.${PROTOCOL_MAJOR_DIR}`;
const SAMPLES_DIR = fileURLToPath(
  new URL(`../../proto/compat/${PROTOCOL_MAJOR_DIR}/samples/`, import.meta.url),
);

// 样本命名：<短消息名>.<n>.binpb，期望值在同名 .json
const SAMPLE_NAME = /^([A-Za-z_][A-Za-z0-9_]*)\.(\d+)\.binpb$/;

type Sample = { file: string; messageName: string; jsonFile: string };

function listSamples(): Sample[] {
  return readdirSync(SAMPLES_DIR)
    .filter((file) => file.endsWith(".binpb"))
    .sort()
    .map((file) => {
      const match = SAMPLE_NAME.exec(file);
      if (match === null) {
        throw new Error(`样本文件名不合规（应为 <短消息名>.<n>.binpb）：${file}`);
      }
      return {
        file,
        messageName: `${PROTO_PACKAGE}.${match[1]}`,
        jsonFile: file.replace(/\.binpb$/, ".json"),
      };
    });
}

function isDescFile(value: unknown): value is DescFile {
  return typeof value === "object" && value !== null && "kind" in value && value.kind === "file";
}

// 生成代码每个 *_pb.ts 导出一个 file_xxx 文件描述符；全部收进注册表，按全名查消息
function buildRegistry() {
  const modules = import.meta.glob<Record<string, unknown>>("./gen/**/*_pb.ts", { eager: true });
  const files = Object.values(modules).flatMap((exports) => Object.values(exports).filter(isDescFile));
  return createRegistry(...files);
}

const samples = listSamples();
const registry = buildRegistry();

describe("金样本跨语言解码", () => {
  // 防空集通过：没有样本时下面的逐条测试一条都不跑，整组会是假绿
  it("样本目录里至少有一个样本", () => {
    expect(samples.length, `${SAMPLES_DIR} 下没有 .binpb 样本`).toBeGreaterThan(0);
  });

  it.each(samples)("$file 解码后的 JSON 与期望一致", ({ file, messageName, jsonFile }) => {
    const schema = registry.getMessage(messageName);
    if (schema === undefined) {
      throw new Error(`${file}：生成代码里找不到消息 ${messageName}`);
    }
    const message = fromBinary(schema, readFileSync(`${SAMPLES_DIR}${file}`));
    const expected: unknown = JSON.parse(readFileSync(`${SAMPLES_DIR}${jsonFile}`, "utf8"));
    expect(toJson(schema, message), file).toEqual(expected);
  });
});
