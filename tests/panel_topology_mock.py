"""Loopback-only topology fixtures. No ADB, hardware, service or config writes."""
import json
import re
import subprocess
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit, parse_qs

PAGE = Path(__file__).resolve().parents[1] / 'web/calibration.html'
CASES = {
    'mini': ('yandexmini', [('logo',6)], False),
    'midi': ('yandexmidi', [('ring',24),('logo',6)], False),
    'pickle': ('yandexmini_3_pro', [('ring',16),('logo',6)], False),
    'mango': ('mango', [('ring',4),('logo',3)], False),
    'orion': ('yandexstation_3', [('ring',36)], False),
    'max': ('yandexstation_max', [('ring',24)], False),
    'future': ('unlisted-board', [('ring',12),('logo',2)], False),
    'station2': ('yandexstation_2', [('ring',89)], True),
    'none': ('no-led-board', [], False),
}
state = {'case':'midi','presets':{},'active':False,'last':None}

class Handler(BaseHTTPRequestHandler):
    def log_message(self,*_): pass
    def send(self,value,code=200):
        body=json.dumps(value).encode();self.send_response(code)
        self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(body)))
        self.end_headers();self.wfile.write(body)
    def do_GET(self):
        url=urlsplit(self.path)
        model,outputs,airplay=CASES[state['case']]
        if url.path in ('/','/calibration'):
            state['case']=parse_qs(url.query).get('fixture',['midi'])[0]
            if state['case'] not in CASES:return self.send({},404)
            state['active']=False
            body=PAGE.read_bytes();self.send_response(200);self.send_header('Content-Type','text/html; charset=utf-8')
            self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body);return
        if url.path=='/api/device':return self.send({'model':model,'room':'OFFLINE FIXTURE'})
        if url.path.startswith('/api/airplay-panel/') and not airplay:return self.send({'error':'AirPlay not installed'},503)
        if url.path.endswith('/status'):
            native=url.path.startswith('/api/led-panel/')
            reason='Station 2 uses AirPlay RGBW backend' if native and airplay else None
            zones=[{'id':name,'count':count,'supported':not reason} for name,count in outputs]
            return self.send({'led_count':outputs[0][1] if outputs and not reason else 0,'zones':zones,
                'unavailable_reason':reason,'defaults':{'spectrum_gain_r':1,'spectrum_gain_g':1,'spectrum_gain_b':1,'timing_mode':'fast_audio'},
                'preview':{'active':state['active'],'allowed':state['active'],'suppressed':'','last_error':''}})
        if url.path.endswith('/settings'):return self.send({'spectrum_gain_r':1,'spectrum_gain_g':1,'spectrum_gain_b':1})
        if url.path.endswith('/presets'):return self.send(state['presets'])
        if url.path=='/fixture-state':return self.send(state)
        self.send({},404)
    def do_POST(self):
        data=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))))
        state['last']=data;state['active']=True
        self.send({'id':'topology-fixture','sent':data['colors'],'ttl_ms':4000})
    def do_DELETE(self):
        self.rfile.read(int(self.headers.get('Content-Length',0)));state['active']=False;self.send({'stopped':True})
    def do_PUT(self):
        self.rfile.read(int(self.headers.get('Content-Length',0)));self.send({'saved':True,'restart_required':False})

if __name__=='__main__':
    script=re.search(r'<script>(.*?)</script>',PAGE.read_text(),re.S).group(1)
    subprocess.run(['node','--check'],input=script,text=True,check=True)
    ThreadingHTTPServer(('127.0.0.1',18798),Handler).serve_forever()
