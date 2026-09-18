#!/usr/bin/env bash
# mail2 开发/测试辅助脚本
# 用法: ./scripts/dev.sh <command>
set -euo pipefail

GREENMAIL_NAME="mail2-greenmail"
GREENMAIL_IMAGE="docker.io/greenmail/standalone:latest"

cmd_greenmail_start() {
  if podman ps -a --format '{{.Names}}' | grep -qx "$GREENMAIL_NAME"; then
    podman start "$GREENMAIL_NAME" >/dev/null
    echo "greenmail 容器已启动: $GREENMAIL_NAME (SMTP 3025 / IMAP 3143 / API 8081)"
  else
    podman run -d --name "$GREENMAIL_NAME" \
      -p 3025:3025 -p 3143:3143 -p 8081:8080 \
      -e GREENMAIL_OPTS="-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.auth.disabled" \
      "$GREENMAIL_IMAGE"
    echo "greenmail 已创建并启动: $GREENMAIL_NAME"
    echo "  测试用端口: SMTP 127.0.0.1:3025, IMAP 127.0.0.1:3143, Web API 127.0.0.1:8081"
    echo "  auth 已禁用：任意账号密码可登录，任意收件地址自动建邮箱"
  fi
}

cmd_greenmail_stop()  { podman stop  "$GREENMAIL_NAME" || true; }
cmd_greenmail_rm()    { podman rm -f "$GREENMAIL_NAME" || true; }
cmd_greenmail_status(){ podman ps --filter "name=$GREENMAIL_NAME" --format '{{.Names}} {{.Status}} {{.Ports}}'; }

cmd_build() { cargo build --release; echo "产物: target/release/mail2"; }
cmd_test()  { TZ="${TZ:-Asia/Shanghai}" cargo test "$@"; }  # 检查时刻测试依赖本地时区
cmd_run()   { exec cargo run -- serve "${@:---config-dir .}"; }
cmd_fetch() { exec cargo run -- fetch-once --config-dir .; }
cmd_check() { exec cargo run -- check-once --config-dir .; }
cmd_init()  { exec cargo run -- init --config-dir .; }

cmd_mock_llm() {
  echo "启动 mock LLM 服务器（127.0.0.1:18765，OpenAI 兼容 /chat/completions）"
  echo "把 llm.yaml 的 base_url 改为 http://127.0.0.1:18765 即可离线联调（不消耗真实 API）"
  exec python3 "$(dirname "$0")/mock_llm.py"
}

usage() {
  cat <<'EOF'
用法: ./scripts/dev.sh <command> [args]
  greenmail-start   启动本地测试邮件服务器（podman greenmail: SMTP 3025/IMAP 3143）
  greenmail-stop    停止
  greenmail-status  查看状态
  greenmail-rm      删除容器
  build             cargo build --release
  test              cargo test（需先 greenmail-start）
  run               cargo run -- serve（使用当前目录 config/llm.yaml）
  fetch             cargo run -- fetch-once
  check             cargo run -- check-once
  init              cargo run -- init（生成 .example 模板）
  mock-llm          启动离线 mock LLM（127.0.0.1:18765）
EOF
}

case "${1:-}" in
  greenmail-start)  cmd_greenmail_start ;;
  greenmail-stop)   cmd_greenmail_stop ;;
  greenmail-status) cmd_greenmail_status ;;
  greenmail-rm)     cmd_greenmail_rm ;;
  build)            cmd_build ;;
  test)             shift; cmd_test "$@" ;;
  run)              shift; cmd_run "$@" ;;
  fetch)            cmd_fetch ;;
  check)            cmd_check ;;
  init)             cmd_init ;;
  mock-llm)         cmd_mock_llm ;;
  *) usage ;;
esac
