import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createBrowserCommandPoller} from '../app/utils/browser-commands.ts';

test('command poller keeps heartbeats during actions and never re-executes after a lost completion',async()=>{
  let finish,executions=0,posts=0,polls=0;
  const replies=[];
  const poller=createBrowserCommandPoller({
    poll:async()=>{polls++;return {request_id:'one',command:{action:'click'}}},
    execute:async()=>{executions++;return new Promise(resolve=>{finish=resolve})},
    complete:async body=>{posts++;if(posts===1) throw Error('lost reply');replies.push(body)},
  });
  await poller.tick();await poller.tick();
  assert.equal(polls,2);assert.equal(executions,1);
  finish({clicked:true});await new Promise(resolve=>setTimeout(resolve,0));
  await poller.tick();await poller.tick();
  assert.equal(executions,1);assert.ok(posts>=2);
  assert.deepEqual(replies[0],{request_id:'one',result:{clicked:true}});
  poller.stop();await poller.tick();assert.equal(polls,4);
});

test('disappearing broker request and disposal cancel in-flight extension confirmations',async()=>{
  let active=true;
  const cancelled=[];
  const poller=createBrowserCommandPoller({
    poll:async()=>({request_id:active?'one':null,command:active?{action:'fill'}:null}),
    execute:()=>new Promise(()=>{}),cancel:id=>cancelled.push(id),complete:async()=>{},
  });
  await poller.tick();active=false;await poller.tick();
  assert.deepEqual(cancelled,['one']);
  poller.stop();assert.deepEqual(cancelled,['one','one']);
});
