const fs=require('node:fs'),vm=require('node:vm'),assert=require('node:assert/strict');
const source=fs.readFileSync(process.argv[2],'utf8');
assert.match(source,/#v-plugin\{max-width:none;width:100%;margin:0;padding:0\}/);
assert.match(source,/#frame\{[^}]*border:0;[^}]*border-radius:0/);
const start=source.indexOf('let panelObserver=');
const end=source.indexOf("window.addEventListener('hashchange',route);",start);
assert(start>=0&&end>start);
let contentHeight=2400,active=true,observed=null;
const events={},style={},root={getBoundingClientRect:()=>({height:contentHeight})};
const doc={querySelector:()=>root,documentElement:{dataset:{}}};
const frame={style,contentDocument:doc,addEventListener:(event,fn)=>{events[event]=fn}};
const view={classList:{contains:()=>active}};
const top={getBoundingClientRect:()=>({height:76})};
const sandbox={
  $:s=>s==='#frame'?frame:s==='#v-plugin'?view:top,
  document:{documentElement:{dataset:{theme:'dark'}}},
  window:{innerHeight:800,addEventListener:()=>{}},
  cancelAnimationFrame:()=>{},requestAnimationFrame:fn=>{fn();return 1},
  ResizeObserver:class{constructor(fn){this.fn=fn}observe(r){observed=r}disconnect(){}}
};
vm.createContext(sandbox);
vm.runInContext(source.slice(start,end),sandbox);
events.load();
assert.equal(style.height,'2400px','panel must use its complete content height');
assert.equal(observed,root);
assert.equal(doc.documentElement.dataset.theme,'dark');
contentHeight=300;
sandbox.resizePanel();
assert.equal(style.height,'724px','short panels fill remaining viewport and can shrink');
contentHeight=1800;
sandbox.resizePanel();
assert.equal(style.height,'1800px');
active=false;contentHeight=100;
sandbox.resizePanel();
assert.equal(style.height,'1800px','hidden route must not collapse the frame');
active=true;
Object.defineProperty(frame,'contentDocument',{get(){throw Error('cross-origin')}});
assert.doesNotThrow(()=>sandbox.resizePanel());
console.log('FULL_AREA_LAYOUT_PASS: expand, shrink, viewport, hidden route, theme, isolation');
