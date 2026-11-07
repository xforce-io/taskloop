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
