# 把 scripts/deps-policy.toml（TOML 的小子集，格式见该文件开头）解析成 JSON。
# 用法：jq -R -s -f deps-policy.jq deps-policy.toml
# 写出子集以外的内容时 error() 点名出错的那段文字，jq 以非零退出。

def crate_keys: ["internal", "direct", "direct_dev", "deny_transitive", "transitive_exempt"];
def array_keys: ["internal", "direct", "direct_dev", "transitive_exempt"];
def modes: ["normal_build", "normal_build_dev", "none"];
def pair_re: "(?<key>[a-z_]+)\\s*=\\s*(?<value>\"[^\"\\n]*\"|\\[[^\\]]*\\])";

# 一节的正文 → {key: value}
def parse_body($header):
  . as $body
  | ($body | gsub(pair_re; "") | gsub("\\s"; "")) as $rest
  | if $rest != "" then error("deps-policy.toml [\($header)] 里有无法解析的内容：\($rest)") else . end
  | [ $body | capture(pair_re; "g")
      | { key: .key, value: (.value | gsub(",\\s*\\]"; "]") | fromjson) } ]
  | from_entries;

def check_crate($name):
  . as $c
  | (keys - crate_keys) as $unknown
  | if ($unknown | length) > 0 then error("deps-policy.toml [crates.\($name)] 有未知的键：\($unknown | join(", "))") else . end
  | if ($c | has("internal") | not) then error("deps-policy.toml [crates.\($name)] 缺少必填的 internal") else . end
  | reduce (array_keys[] | select(. as $k | $c | has($k))) as $k (.;
      if ($c[$k] | type) != "array" or any($c[$k][]; type != "string")
      then error("deps-policy.toml [crates.\($name)] 的 \($k) 必须是字符串数组") else . end)
  | if has("deny_transitive") and (.deny_transitive as $m | modes | index($m) | not)
    then error("deps-policy.toml [crates.\($name)] 的 deny_transitive 只能是 \(modes | join(" / "))，现在是 \(.deny_transitive)")
    else . end;

# 去注释（策略里的字符串不含 #）
[ split("\n")[] | sub("#.*$"; "") ] | join("\n")
# 按表头切节：第一段是表头之前的内容，必须为空
| [ splits("(^|\\n)\\s*\\[(?=[a-z])") ]
| if (.[0] | gsub("\\s"; "")) != "" then error("deps-policy.toml 在第一个表头之前有内容") else . end
| .[1:]
| map(capture("^(?<header>[a-z0-9_.-]+)\\]\\s*(?<body>[\\s\\S]*)$")
      // error("deps-policy.toml 表头写法不对：\(.)"))
| reduce .[] as $s ({ workspace: null, crates: {} };
    ($s.body | parse_body($s.header)) as $kv
    | if $s.header == "workspace" then .workspace = $kv
      elif ($s.header | startswith("crates.")) then
        ($s.header | ltrimstr("crates.")) as $n
        | if .crates | has($n) then error("deps-policy.toml [crates.\($n)] 重复") else . end
        | .crates[$n] = ($kv | check_crate($n))
      else error("deps-policy.toml 未知的表头 [\($s.header)]") end)
| if .workspace == null or (.workspace.io_and_framework | type) != "array"
  then error("deps-policy.toml 缺少 [workspace] io_and_framework") else . end
