#!/usr/bin/env node
// Fake Pi wire protocol for the Rust -> real Milkie -> harness boundary test.
const fs=require('node:fs'),net=require('node:net'),crypto=require('node:crypto')
const args=process.argv.slice(2),file=args[args.indexOf('--session')+1]
let input=''
const emit=x=>process.stdout.write(JSON.stringify(x)+'\n')
process.stdin.setEncoding('utf8');process.stdin.on('data',s=>input+=s)
process.stdin.on('end',()=>{
 const invocation=JSON.parse(input),id=crypto.randomUUID()
 const header={type:'session',id,cwd:process.cwd()}
 fs.writeFileSync(file,JSON.stringify(header)+'\n');emit(header)
 if(invocation.flow_input.hold){setInterval(()=>{},1000);return}
 if(invocation.flow_input.nonce==='ITERATION-LIMIT') {
  const extension=args[args.indexOf('--extension')+1]
  if(!extension) throw new Error('Missing real Milkie extension')
  // Exercise the generated enforcement hook with a deterministic Pi API, without a provider.
  // Type conversion and socket tools are registered but never called by this no-tool loop.
  const source=fs.readFileSync(extension,'utf8').replace(/^import .*$/mg,'').replace('export default function (pi)', 'return function (pi)')
  const hooks=new Map(),requests=[]
  const pi={on:(event,fn)=>hooks.set(event,fn),registerTool:()=>{}}
  new Function('writeFileSync','net','Type',source)(fs.writeFileSync,net,{Unsafe:x=>x})(pi)
  const hook=hooks.get('before_provider_request');if(!hook) throw new Error('Missing iteration hook')
  let aborted=false
  for(let n=0;n<20;n++) {
   hook({}, {abort:()=>{aborted=true}})
   if(aborted)break
   requests.push({request:n+1})
  }
  fs.writeFileSync('provider-requests.json',JSON.stringify(requests))
  if(!aborted) throw new Error('Iteration limit did not abort')
  emit({type:'message_end',message:{role:'assistant',content:[],stopReason:'aborted'}})
  emit({type:'agent_end',messages:[]});return
 }

 const name=Object.keys(invocation.tool_mapping).find(k=>invocation.tool_mapping[k]==='artifact.write')
 const socket=net.connect(process.env.MILKIE_TOOL_SOCKET)
 let buffer=''
 socket.on('error',()=>process.exit(2));socket.on('data',s=>{buffer+=s;if(!buffer.includes('\n'))return
  const result=JSON.parse(buffer.split('\n')[0]);socket.end()
  emit({type:'message_end',message:{role:'assistant',content:[{type:'text',text:result.ok?'candidate submitted':'host rejected'}],stopReason:'stop'}})
  emit({type:'agent_end',messages:[]})
 })
 socket.on('connect',()=>socket.write(JSON.stringify({id:'model-request',nativeCallId:crypto.randomUUID(),name,input:{slot:'result',value:{nonce:invocation.flow_input.nonce}}})+'\n'))
})
