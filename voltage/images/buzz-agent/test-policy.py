"""Packaged development harness wire checks; fictional tasks and no credentials."""
import subprocess
import sys

IMAGE = sys.argv[1]
SCRIPT = r'''const fs = require('node:fs'), cp = require('node:child_process');
const fake = `
const fs=require('node:fs'),rl=require('node:readline');
const send=v=>console.log(JSON.stringify({jsonrpc:'2.0',...v}));
let prompt;
rl.createInterface({input:process.stdin}).on('line',line=>{
 const m=JSON.parse(line);fs.appendFileSync('/tmp/wire',JSON.stringify(m)+'\\n');
 if(m.method==='initialize')send({id:m.id,result:{protocolVersion:2,agentCapabilities:{},authMethods:[]}});
 else if(m.method==='session/new')send({id:m.id,result:{sessionId:'synthetic',modes:{availableModes:process.env.CASE==='unsupported'?[]:[{id:process.env.MODE}]},configOptions:[]}});
 else if(m.method==='session/set_config_option') {
  if(process.env.CASE==='refused')send({id:m.id,error:{code:-32602,message:'refused'}});
  else send({id:m.id,result:{configOptions:process.env.CASE==='unconfirmed'?[]:[{id:'mode',currentValue:process.env.MODE}]}});
 }else if(m.method==='session/prompt'){
  prompt=m.id;send({id:'fictional-request',method:'session/request_permission',params:{sessionId:'synthetic',toolCall:{toolCallId:'fictional',title:'Fictional escalation',kind:'execute'},options:[{optionId:'grant',kind:'allow_once',name:'Grant'}]}});
 }else if(m.id==='fictional-request')send({id:prompt,result:{stopReason:'end_turn'}});
 else if(m.method&&m.id!==undefined)send({id:m.id,result:{}});
});`;
fs.writeFileSync('/tmp/fake.js',fake);
// Preserve the actual standard-adapter command identity while using a fictional wire peer.
fs.symlinkSync('/usr/local/bin/node','/tmp/codex-acp');
const mode='agent-full-access';
fs.writeFileSync('/tmp/task.json',JSON.stringify({version:1,taskId:'policy-check',agentPubkey:'79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798',prompt:'Synthetic developer task',maxDurationMs:5000}));
for(const scenario of ['confirmed','unsupported','refused','unconfirmed']){
 fs.writeFileSync('/tmp/wire','');
 const r=cp.spawnSync('/usr/local/bin/buzz-acp',['run','--no-memory','--task','/tmp/task.json','--relay-url','ws://127.0.0.1:1','--agent-command','/tmp/codex-acp','--agent-args','/tmp/fake.js'],{encoding:'utf8',timeout:15000,env:{...process.env,MODE:mode,CASE:scenario,BUZZ_PRIVATE_KEY:'0'.repeat(63)+'1',BUZZ_ACP_MCP_COMMAND:'buzz-dev-mcp',BUZZ_ACP_PERMISSION_MODE:mode}});
 const frames=fs.readFileSync('/tmp/wire','utf8').trim().split('\n').filter(Boolean).map(JSON.parse);
 const session=frames.find(x=>x.method==='session/new');
 if(!session||session.params.mcpServers.length===0)throw Error(JSON.stringify({reason:'missing session or developer MCP unavailable',mode,scenario,status:r.status,stderr:r.stderr,stdout:r.stdout,frames}));
 const prompts=frames.filter(x=>x.method==='session/prompt');
 if(scenario==='confirmed'){
  const response=frames.find(x=>x.id==='fictional-request'&&x.result);
  if(r.status!==0||prompts.length!==1||response?.result?.outcome?.outcome!=='selected'||response?.result?.outcome?.optionId!=='grant')throw Error(JSON.stringify({mode,scenario,status:r.status,stderr:r.stderr,response}));
 }else if(r.status!==1||prompts.length!==0)throw Error(JSON.stringify({reason:'mode refusal did not fail before prompt',mode,scenario,status:r.status,stderr:r.stderr}));
 console.log('PASS: '+mode+' '+scenario+'; developer MCP present; tool request approved after mode confirmation, otherwise prompt refused');
}
'''
result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none', '--cap-drop', 'ALL',
                         '--security-opt', 'no-new-privileges', '--entrypoint', 'node', IMAGE],
                        input=SCRIPT, text=True, capture_output=True, timeout=150)
print(result.stdout, end='')
print(result.stderr, end='', file=sys.stderr)
sys.exit(result.returncode)
