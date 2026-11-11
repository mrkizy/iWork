// 侧栏项目的展开状态：按项目记在本地（方案 2.5「项目展开状态（按项目记在本地）」）。
// 只是界面偏好，读写失败（例如存储被禁用）就当没记过，不影响任何业务。
const KEY = "iwork.sidebar.expanded";

export function loadExpanded(): Record<string, boolean> {
  try {
    const raw = localStorage.getItem(KEY);
    const v: unknown = raw === null ? {} : JSON.parse(raw);
    return typeof v === "object" && v !== null ? (v as Record<string, boolean>) : {};
  } catch {
    return {};
  }
}

export function saveExpanded(v: Record<string, boolean>) {
  try {
    localStorage.setItem(KEY, JSON.stringify(v));
  } catch {
    // 存不了就只在这次运行里记着
  }
}
