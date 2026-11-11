// 界面传输不可用时的固定诊断页（方案 2.4 第 7 条）：文字写死，不依赖任何帧数据。
import type { TransportFailure } from "../bridge/frames";
import "./TransportFailure.css";

const DETAIL: Record<TransportFailure["kind"], string> = {
  fallback: "窗口和 iWork 之间的通道被挡住了，界面拿不到数据。",
  not_array_buffer: "窗口和 iWork 之间的通道返回了意外的数据格式。",
  closed: "iWork 的界面会话已经结束。",
  error: "窗口和 iWork 之间的通道出错了。",
};

export function TransportFailurePage(props: { failure: TransportFailure }) {
  return (
    <main class="iw-transport-failure" role="alert">
      <h1>界面传输不可用，请重新打开 iWork</h1>
      <p>{DETAIL[props.failure.kind]}</p>
      <p class="iw-transport-failure__logs">
        日志在 iWork 数据目录的 logs 文件夹里：macOS 是 ~/Library/Application Support/iWork/logs/，Linux 是 $XDG_DATA_HOME/iwork/logs/。
      </p>
    </main>
  );
}
