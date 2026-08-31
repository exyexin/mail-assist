#!/usr/bin/env python3
"""mail2 离线联调用 mock LLM：OpenAI 兼容 /chat/completions。
按邮件内容返回脚本化 JSON：面试/会议→todo，账单/公告→notification，讨论→conversation，其余→misc。
提取：面试→{字节跳动/技术面试/2027-09-10T14:00+08:00}，会议→{Acme 公司/项目评审会/…}。
用法: python3 mock_llm.py [port]，默认 18765。"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18765


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        messages = body.get("messages", [])
        system = next((m["content"] for m in messages if m.get("role") == "system"), "")
        user = next((m["content"] for m in messages if m.get("role") == "user"), "")

        if "提取" in system:
            if "没有时间" in user:
                content = {"party": "某公司", "event": "待定事项", "title": "待定事项", "deadline": None, "category": None}
            elif "面试" in user:
                content = {"party": "字节跳动", "event": "技术面试", "title": "技术面试邀请", "deadline": "2027-09-10T14:00:00+08:00", "category": None}
            elif "会议" in user:
                content = {"party": "Acme 公司", "event": "项目评审会", "title": "项目评审会议", "deadline": "2027-09-20T10:00:00+08:00", "category": None}
            else:
                content = {"party": "未知", "event": "一般事项", "title": "一般事项", "deadline": None, "category": None}
        elif "提醒邮件回复" in system:
            if "已完成" in user:
                content = {"intent": "done"}
            elif "不再提醒" in user:
                content = {"intent": "silent"}
            else:
                content = {"intent": "none"}
        else:
            if "面试" in user or "会议" in user:
                content = {"category": "todo"}
            elif "账单" in user or "公告" in user:
                content = {"category": "notification"}
            elif "讨论" in user:
                content = {"category": "conversation"}
            else:
                content = {"category": "misc"}

        resp = json.dumps({"choices": [{"message": {"role": "assistant", "content": json.dumps(content, ensure_ascii=False)}}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def log_message(self, fmt, *args):
        print("[mock-llm]", fmt % args)


if __name__ == "__main__":
    print(f"mock LLM 监听 http://127.0.0.1:{PORT}/chat/completions")
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
