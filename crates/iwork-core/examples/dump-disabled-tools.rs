//! 导出产品会话的永久禁用工具名单（T06 方案 3.4「不问工具」记录行），供 `tools/t0/iwork-t0g.py --disabled-tools` 用。
//! 名单不在脚本里另抄一份：这里调的就是产品启动参数用的 `tools_to_disable`，对象是「全部分组都开」的助手会话，
//! 所以结果只剩永久禁用的那几个（登记表里归为 `Disabled` 的工具）。
//!
//! 用法：`cargo run -p iwork-core --example dump-disabled-tools > disabled-tools.json`

use iwork_core::approval::session::{tool_set_for, tools_to_disable};
use iwork_domain::common::ToolGroups;
use iwork_domain::turn::TurnKind;

fn main() {
    let set = tool_set_for(TurnKind::Assistant, &ToolGroups::new());
    let names = tools_to_disable(&set);
    println!(
        "{}",
        serde_json::to_string_pretty(&names).expect("工具名可序列化")
    );
}
