// Plugin pages: full-area layout in the Luna UI and the sandboxed panel bridge.
// Usage: node tests/web_full_area.cjs web/index.html
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm'),assert=require('node:assert/strict');
// Objects built inside a vm context have that context's prototypes.
const same=(actual,expected,message)=>assert.equal(JSON.stringify(actual),JSON.stringify(expected),message);
const source=fs.readFileSync(process.argv[2],'utf8');
assert.match(source,/#v-plugin\{max-width:none;width:100%;margin:0;padding:0\}/);
assert.match(source,/#frame\{[^}]*border:0;[^}]*border-radius:0/);
const frameTag=source.match(/<iframe id="frame"[^>]*>/);
assert(frameTag,'plugin frame is present');
assert.match(frameTag[0],/sandbox="[^"]*allow-scripts/,'plugin frame is sandboxed');
assert.doesNotMatch(frameTag[0],/allow-same-origin/,'plugin pages must not share the Luna origin');
assert.doesNotMatch(source,/contentDocument/,'the UI must not reach into the plugin document');

async function shell(){
  const start=source.indexOf('let panelHeight=');
  const end=source.indexOf("window.addEventListener('hashchange',route);",start);
  assert(start>=0&&end>start);
  let active=true;
  const posted=[],requests=[],listeners={},frameEvents={},style={};
  const panel={postMessage:(message,origin,transfer)=>posted.push({message,origin,transfer})};
  const frame={style,contentWindow:panel,addEventListener:(event,fn)=>{frameEvents[event]=fn}};
  const view={classList:{contains:()=>active}};
  const top={getBoundingClientRect:()=>({height:76})};
  const context={
    $:s=>s==='#frame'?frame:s==='#v-plugin'?view:top,
    document:{documentElement:{dataset:{theme:'dark'}}},
    window:{innerHeight:800,addEventListener:(event,fn)=>{listeners[event]=fn}},
    cancelAnimationFrame:()=>{},requestAnimationFrame:fn=>{fn();return 1},
    fetch:async(url,init)=>{
      requests.push({url,init});
      return{status:200,statusText:'OK',headers:new Map([['content-type','application/json']]),
        arrayBuffer:async()=>new TextEncoder().encode('{"ok":true}').buffer};
    },
    TextEncoder,
  };
  vm.createContext(context);
  vm.runInContext(source.slice(start,end),context);
  const send=(data,from=panel)=>listeners.message({source:from,data});
  const settle=()=>new Promise(resolve=>setImmediate(resolve));

  frameEvents.load();
  same(posted.pop().message,{luna:'theme',theme:'dark'},'theme goes to the panel on load');
  send({luna:'size',height:2400});
  assert.equal(style.height,'2400px','panel must use its complete content height');
  send({luna:'size',height:300});
  assert.equal(style.height,'724px','short panels fill remaining viewport and can shrink');
  send({luna:'size',height:1800});
  assert.equal(style.height,'1800px');
  active=false;
  send({luna:'size',height:100});
  assert.equal(style.height,'1800px','hidden route must not collapse the frame');
  active=true;
  send({luna:'size',height:50},{});
  assert.equal(style.height,'1800px','messages from other windows are ignored');
  send({luna:'ready'});
  assert.equal(posted.pop().message.luna,'theme');

  // /api calls are forwarded with the page's package id; the page cannot pick it.
  vm.runInContext("framePlugin='airplay.panel'",context);
  send({luna:'fetch',id:7,path:'/api/plugins/airplay.panel-command/invoke',method:'POST',
    headers:{'content-type':'application/json','X-Luna-Panel':'luna.admin'},body:'{"args":[]}'});
  await settle();
  assert.equal(requests.length,1);
  assert.equal(requests[0].url,'/api/plugins/airplay.panel-command/invoke');
  assert.equal(requests[0].init.method,'POST');
  assert.equal(requests[0].init.headers['X-Luna-Panel'],'airplay.panel');
  assert.equal(Object.keys(requests[0].init.headers).filter(k=>k.toLowerCase()==='x-luna-panel').length,1);
  const result=posted.pop();
  assert.equal(result.message.luna,'fetch-result');
  assert.equal(result.message.id,7);
  assert.equal(result.message.status,200);
  assert.equal(new TextDecoder().decode(result.message.body),'{"ok":true}');
  assert.equal(result.transfer.length,1);
  assert.equal(result.transfer[0],result.message.body);
  for(const bad of ['/etc/passwd','https://evil.example/api/x','//evil.example/api/x']){
    send({luna:'fetch',id:8,path:bad});
    await settle();
    assert.equal(requests.length,1,bad+' must not be fetched');
    assert.match(posted.pop().message.error,/invalid panel request/);
  }
}

async function bridge(){
  const code=fs.readFileSync(path.join(__dirname,'..','web','luna-panel-bridge.js'),'utf8');
  assert.doesNotMatch(code,/<\/script/i,'the bridge is inlined into <script>');
  const posted=[],listeners={},native=[];
  const host={postMessage:(message,origin)=>posted.push({message,origin})};
  let observed=null;
  const root={getBoundingClientRect:()=>({height:512})};
  const context={
    URL,Headers,Response,Request,Blob,URLSearchParams,ArrayBuffer,Promise,TypeError,String,Math,
    location:{host:'192.168.1.5:8099'},
    document:{baseURI:'http://192.168.1.5:8099/web/airplay.panel',readyState:'complete',
      documentElement:{dataset:{}},querySelector:s=>s==='.wrap'?root:null,addEventListener:()=>{}},
    ResizeObserver:class{constructor(fn){this.fn=fn}observe(target){observed=target}},
  };
  context.window=context;
  context.parent=host;
  context.fetch=async(input)=>{native.push(String(input));return new Response('native')};
  context.addEventListener=(event,fn)=>{listeners[event]=fn};
  vm.createContext(context);
  vm.runInContext(code,context);
  const deliver=(data,from=host)=>listeners.message({source:from,data});

  assert.equal(observed,root,'the bridge watches .wrap');
  same(posted.shift().message,{luna:'size',height:512});
  same(posted.shift().message,{luna:'ready'});

  const pending=context.fetch('/api/device');
  const request=posted.shift().message;
  assert.equal(request.luna,'fetch');
  assert.equal(request.path,'/api/device');
  assert.equal(request.method,'GET');
  deliver({luna:'fetch-result',id:request.id+1,status:500,body:'wrong id'});
  deliver({luna:'fetch-result',id:request.id,status:200,body:'{"model":"x"}'},{});
  deliver({luna:'fetch-result',id:request.id,status:200,headers:{'content-type':'application/json'},
    body:new TextEncoder().encode('{"model":"yandexmidi"}').buffer});
  const response=await pending;
  assert.equal(response.status,200);
  assert.equal((await response.json()).model,'yandexmidi');

  const post=context.fetch('http://192.168.1.5:8099/api/http?x=1',{method:'post',
    headers:{'Content-Type':'application/json'},body:'{}'});
  const forwarded=posted.shift().message;
  assert.equal(forwarded.path,'/api/http?x=1');
  assert.equal(forwarded.method,'POST');
  assert.equal(forwarded.headers['content-type'],'application/json');
  assert.equal(forwarded.body,'{}');
  deliver({luna:'fetch-result',id:forwarded.id,status:204,body:new ArrayBuffer(0)});
  assert.equal((await post).status,204);

  const failing=context.fetch('/api/plugins');
  deliver({luna:'fetch-result',id:posted.shift().message.id,error:'plugin page airplay.panel may not call'});
  await assert.rejects(failing,/may not call/);

  await assert.rejects(context.fetch('/api/plugins',{method:'POST',body:{form:'data'}}),/supports string/);
  assert.equal(posted.length,0,'unsupported bodies are not sent');

  assert.equal(await (await context.fetch('/luna-panel.css')).text(),'native');
  assert.equal(await (await context.fetch('https://example.org/api/x')).text(),'native');
  assert.deepEqual(native,['/luna-panel.css','https://example.org/api/x']);
  assert.equal(posted.length,0,'only same-host /api calls use the bridge');

  deliver({luna:'theme',theme:'light'});
  assert.equal(context.document.documentElement.dataset.theme,'light');
  deliver({luna:'theme',theme:'dark'},{});
  assert.equal(context.document.documentElement.dataset.theme,'light','other windows cannot set the theme');
}

(async()=>{
  await shell();
  await bridge();
  console.log('FULL_AREA_LAYOUT_PASS: expand, shrink, viewport, hidden route, theme, sandbox, panel bridge');
})().catch(error=>{console.error(error);process.exit(1)});
