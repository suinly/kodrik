import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createFormSaveQueue} from '../app/utils/ui-form.ts';

const deferred=()=>{let resolve,reject;const promise=new Promise((yes,no)=>{resolve=yes;reject=no});return {promise,resolve,reject}};
test('form saves serialize edits and build the next request after receiving its revision',async()=>{
  let value='first',revision=null;const calls=[],requests=[];let done;
  const queue=createFormSaveQueue({build:()=>({value,revision}),send:request=>{requests.push(request);const gate=deferred();calls.push(gate);return gate.promise},saved:result=>{revision=result.revision},failed:error=>{throw error}});
  done=queue.change();assert.equal(requests.length,1);
  value='second';await queue.change();assert.equal(requests.length,1);
  calls[0].resolve({revision:1});await new Promise(resolve=>setImmediate(resolve));
  assert.equal(requests.length,2);assert.equal(requests[1].revision,1);assert.equal(requests[1].value,'second');
  calls[1].resolve({revision:2});await done;assert.equal(revision,2);
});
test('uncertain saves retry the identical request before sending later edits',async()=>{
  let value='first',fail=true;const requests=[],errors=[];
  const queue=createFormSaveQueue({build:()=>({value}),send:async request=>{requests.push(request);if(fail) throw Error('offline');return {}},saved:()=>{},failed:error=>errors.push(error.message)});
  await queue.change();value='second';fail=false;await queue.change();
  assert.equal(requests.length,3);assert.deepEqual(requests[0],requests[1]);assert.equal(requests[2].value,'second');assert.notEqual(requests[0].request_id,requests[2].request_id);assert.deepEqual(errors,['offline']);
});
test('form queues remain idle on construction and can retry retained requests after reload',async()=>{
  const pending={request_id:'retained',value:'first'},requests=[];
  const queue=createFormSaveQueue({pending,build:()=>({value:'second'}),send:async request=>{requests.push(request);return {}},saved:()=>{},failed:error=>{throw error}});
  assert.deepEqual(requests,[]);await queue.retry();assert.deepEqual(requests,[pending]);
});

test('confirmed invalid requests can be corrected without retrying rejected values',async()=>{
  let value='invalid';const requests=[];
  const queue=createFormSaveQueue({build:()=>({value}),send:async request=>{requests.push(request);if(request.value==='invalid') throw Object.assign(Error('validation'),{definitive:true});return {}},saved:()=>{},failed:()=>{}});
  await queue.change();value='correct';await queue.change();assert.deepEqual(requests.map(request=>request.value),['invalid','correct']);
});

test('retry rebuilds requests rejected by server validation or local required fields',async()=>{
  let valid=false,attempts=0;const requests=[];
  const queue=createFormSaveQueue({build:()=>{if(!valid) throw Error('required');return {value:'fixed'}},send:async request=>{requests.push(request);if(!attempts++) throw Object.assign(Error('validation'),{definitive:true});return {}},saved:()=>{},failed:()=>{}});
  await queue.change();valid=true;await queue.retry();await queue.retry();assert.equal(requests.length,2);assert.notEqual(requests[0].request_id,requests[1].request_id);
});
