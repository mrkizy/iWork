# 依赖方向检查的判定逻辑（由 scripts/check-deps.sh 调用）。
# 输入：`cargo metadata --format-version 1` 的 JSON；$policy：deps-policy.toml 解析后的 JSON。
# 输出：每条违规一行文字；策略本身有问题时以 "POLICY_ERROR:" 开头。没有输出 = 通过。

def kind_name: if . == null then "normal" else . end;

# 包 id → 包名
def names: reduce .packages[] as $p ({}; .[$p.id] = $p.name);

# 包 id → resolve 节点
def nodes: reduce .resolve.nodes[] as $n ({}; .[$n.id] = $n);

# 从 $root 出发求依赖闭包，返回 {子 id: 父 id}（$root 映射到 null）。
# $first_kinds：$root 自己的哪些依赖种类算进来；更深的节点只算 normal 与 build（cargo 不会构建依赖的 dev 依赖）。
# $stop：包名集合，碰到就记下但不再往下走（豁免的框架连同它带进来的依赖都不查）。
def closure($nodes; $names; $root; $first_kinds; $stop):
  { parent: { ($root): null }, queue: [$root] }
  | until(.queue | length == 0;
      .queue[0] as $cur
      | .queue |= .[1:]
      | (if $cur == $root then $first_kinds else ["normal", "build"] end) as $kinds
      | reduce (($nodes[$cur].deps // [])[]
                | select(any(.dep_kinds[]; (.kind | kind_name) as $k | $kinds | index($k)))
                | .pkg) as $child (.;
          if .parent | has($child) then .
          else .parent[$child] = $cur
               | if ($stop | index($names[$child])) then . else .queue += [$child] end
          end))
  | .parent;

# 从闭包的父指针还原路径："iwork-domain 经 ulid → tokio"
def path_text($parent; $names; $id):
  [ $id | recurse($parent[.]; . != null) ] | reverse | map($names[.])
  | if length <= 2 then "\(.[0]) → \(.[-1])"
    else "\(.[0]) 经 \(.[1:-1] | join(" → ")) → \(.[-1])" end;

. as $meta
| names as $names
| nodes as $nodes
| ($meta.workspace_members | map($names[.])) as $ws_names
| ($meta.workspace_members | map({ key: $names[.], value: . }) | from_entries) as $ws_id
| $policy.workspace.io_and_framework as $io
| ($policy.crates | keys) as $policy_names
| (
    # 策略与工作区必须一一对应：加 crate 就必须加策略，删 crate 就必须删策略
    ( $ws_names - $policy_names | .[] | "POLICY_ERROR: 工作区 crate \(.) 在策略里没有 [crates.\(.)] 一节" ),
    ( $policy_names - $ws_names | .[] | "POLICY_ERROR: 策略里的 [crates.\(.)] 不是工作区 crate" ),

    ( $ws_names[] as $crate
      | $policy.crates[$crate] as $p
      | select($p != null)
      | $ws_id[$crate] as $id
      | ($meta.packages[] | select(.id == $id)) as $pkg
      | "策略 scripts/deps-policy.toml [crates.\($crate)]" as $where
      | (
          # 规则一：工作区内部（按闭包）
          ( closure($nodes; $names; $id; ["normal", "build"]; []) as $parent
            | $parent | keys[] | select(. != $id)
            | select($names[.] as $n | ($ws_names | index($n)) and ($p.internal | index($n) | not))
            | "crate \($crate)：\(path_text($parent; $names; .)) 依赖了工作区 crate \($names[.])（规则：工作区内部），\($where)" ),

          # 规则二：直接依赖许可（只看自己的 Cargo.toml）
          ( select($p.direct != null)
            | $pkg.dependencies[]
            | select((.kind | kind_name) != "dev")
            | select(.name as $n | ($ws_names | index($n) | not) and ($p.direct | index($n) | not))
            | "crate \($crate)：直接依赖了 \(.name)（\(.kind | kind_name)），不在 direct 白名单里（规则：直接许可），\($where)" ),
          ( select($p.direct_dev != null)
            | $pkg.dependencies[]
            | select(.kind == "dev")
            | select(.name as $n | ($ws_names | index($n) | not) and ($p.direct_dev | index($n) | not))
            | "crate \($crate)：dev 直接依赖了 \(.name)，不在 direct_dev 白名单里（规则：直接许可），\($where)" ),

          # 规则三：传递禁用（按闭包，豁免的框架不往下走）
          ( ($p.deny_transitive // "normal_build") as $mode
            | select($mode != "none")
            | ($p.transitive_exempt // []) as $exempt
            | (if $mode == "normal_build_dev" then ["normal", "build", "dev"] else ["normal", "build"] end) as $kinds
            | closure($nodes; $names; $id; $kinds; $exempt) as $parent
            | $parent | keys[] | select(. != $id)
            | select($names[.] as $n | ($io | index($n)) and ($exempt | index($n) | not))
            | "crate \($crate)：\(path_text($parent; $names; .)) 依赖了 \($names[.])（规则：传递禁用，范围 \($mode)），\($where)" )
        )
    )
  )
