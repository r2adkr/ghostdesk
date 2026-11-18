#!/usr/bin/env python3
"""Exercise the production Rust CLI adapter using Codex CLI and a local, inference-free mock Responses endpoint."""
import argparse
import http.server
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import threading

WRAPPER = '''#!/usr/bin/env python3
import json, os, sys
args=sys.argv[1:]
real=os.environ["GHOSTDESK_FIXTURE_REAL_CODEX"]
if args==["login","status"]:
 print("Logged in using ChatGPT"); sys.exit(0)
if args[0]=="app-server":
 for line in sys.stdin:
  req=json.loads(line)
  if req["method"]=="initialize": print(json.dumps({"id":req["id"],"result":{}}),flush=True)
  if req["method"]=="model/list": print(json.dumps({"id":req["id"],"result":{"data":[{"model":os.environ["GHOSTDESK_FIXTURE_MODEL"]}],"nextCursor":None}}),flush=True)
 sys.exit(0)
if args==["debug","models"]: os.execv(real,[real,"debug","models","--bundled"])
if args[0]=="exec":
 settings=["-c",'model_provider="ghostdesk_review"',"-c",'model_providers.ghostdesk_review={name="Review",base_url="'+os.environ["GHOSTDESK_FIXTURE_ENDPOINT"]+'",wire_api="responses",supports_websockets=false,request_max_retries=0,stream_max_retries=0}']
 os.execv(real,[real,*args[:1],*settings,*args[1:]])
raise RuntimeError("Unsupported fixture mode")
'''

class Endpoint(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        names = [item.get('name', item.get('type')) for item in body.get('tools', [])]
        for item in body.get('input', []):
            if isinstance(item, dict) and item.get('type') == 'additional_tools':
                names.extend(t.get('name', t.get('type')) for t in item['tools'])
        self.server.received.append((body['model'], names))
        answer = json.dumps({'message':'GhostDesk CLI OK', 'calls':[]})
        item = {'type':'message','id':'msg_fixture','role':'assistant','status':'completed','content':[{'type':'output_text','text':answer,'annotations':[]}]}
        response = {'id':'resp_fixture','object':'response','status':'completed','output':[item],'usage':{'input_tokens':14,'output_tokens':12,'total_tokens':26}}
        events = [
            {'type':'response.created','response':dict(response,status='in_progress',output=[])},
            {'type':'response.output_item.added','output_index':0,'item':dict(item,content=[],status='in_progress')},
            {'type':'response.output_text.delta','output_index':0,'content_index':0,'item_id':'msg_fixture','delta':answer},
            {'type':'response.output_item.done','output_index':0,'item':item},
            {'type':'response.completed','response':response},
        ]
        self.send_response(200)
        self.send_header('Content-Type','text/event-stream')
        self.end_headers()
        for event in events:
            self.wfile.write(('event: '+event['type']+'\ndata: '+json.dumps(event)+'\n\n').encode())

    def log_message(self, *args): pass

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('models', nargs='*', default=['gpt-5.5','gpt-6.1-sol'])
    parser.add_argument("--codex", help="Path to the installed Codex executable")
    options=parser.parse_args()
    real=options.codex or shutil.which('codex')
    assert real, 'Codex CLI is required'
    with tempfile.TemporaryDirectory(prefix='ghostdesk-fixture-') as d, http.server.ThreadingHTTPServer(('127.0.0.1',0),Endpoint) as server:
        wrapper=pathlib.Path(d)/'fixture'
        wrapper.write_text(WRAPPER)
        wrapper.chmod(0o700)
        (pathlib.Path(d)/"codex-home").mkdir()
        server.received=[]
        threading.Thread(target=server.serve_forever,daemon=True).start()
        passed=True
        for model in options.models:
            server.received.clear()
            env=dict(os.environ, CODEX_HOME=str(pathlib.Path(d)/'codex-home'), GHOSTDESK_DATA_DIR=str(pathlib.Path(d)/'data'), GHOSTDESK_TEST_CODEX=str(wrapper), GHOSTDESK_FIXTURE_REAL_CODEX=real, GHOSTDESK_FIXTURE_MODEL=model,
                     GHOSTDESK_FIXTURE_ENDPOINT='http://127.0.0.1:'+str(server.server_port)+'/v1')
            run=subprocess.run(['cargo','test','cli::tests::live_codex_model_transport','--','--ignored','--exact'],env=env,text=True,capture_output=True,timeout=180)
            valid=(run.returncode==0 and '1 passed' in run.stdout and server.received==[(model,[])])
            print(model, 'PASS' if valid else 'FAIL', server.received, flush=True)
            if run.returncode: print((run.stdout+run.stderr)[-2200:],file=sys.stderr)
            passed &= valid
        server.shutdown()
    return 0 if passed else 1
if __name__=='__main__': sys.exit(main())
