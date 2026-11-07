import {test} from 'node:test';
import assert from 'node:assert/strict';
import {tokenUsageSummary} from '../app/utils/token-usage.ts';

test('unknown usage is unavailable, not zero',()=>{
  assert.deepEqual(tokenUsageSummary(null,null),{total:null,context:null,percent:null,partial:true});
});
test('context percentage uses a known model limit, never an invented limit',()=>{
  const usage={input_tokens:1000,output_tokens:200,cached_tokens:500,total_tokens:1200,requests:2,partial:false};
  assert.deepEqual(tokenUsageSummary(usage,25000,100000),{total:1200,context:25000,percent:25,partial:false});
  assert.equal(tokenUsageSummary(usage,25000,null).percent,null);
});
test('partial totals stay labelled partial and zero is preserved',()=>{
  const usage={input_tokens:0,output_tokens:null,cached_tokens:null,total_tokens:null,requests:1,partial:true};
  assert.deepEqual(tokenUsageSummary(usage,0,0),{total:null,context:0,percent:null,partial:true});
});
