import {test} from 'node:test';
import assert from 'node:assert/strict';
import {serverURL, truncateUTF8, pageContext} from '../core.mjs';

test('server address keeps deployment prefix and rejects credentials, queries and non-web schemes',()=>{
  assert.equal(serverURL('https://example.com/kodrik').href,'https://example.com/kodrik/');
  for(const value of ['javascript:alert(1)','https://user:secret@example.com','https://example.com/?token=x','https://example.com/#x','http://example.com','http://[::1]:8080/']) assert.throws(()=>serverURL(value));
  assert.equal(serverURL('http://127.0.0.1:8080/').origin,'http://127.0.0.1:8080');
});
test('UTF-8 truncation never splits unicode or exceeds budget',()=>{
  assert.equal(truncateUTF8('абв🙂',7),'абв');
  assert.equal(truncateUTF8('abc',3),'abc');
  assert.equal(truncateUTF8('🙂',3),'');
});
test('page context is structured and serialized byte-bounded including escaped characters',()=>{
  const context=pageContext({url:'https://example.com',title:'Example',text:'я\"\\'.repeat(90000),selection:false});
  assert.ok(new TextEncoder().encode(JSON.stringify(context)).length<=100000);
  assert.equal(context.url,'https://example.com/');assert.equal(context.source,'page');assert.equal(context.truncated,true);
  assert.ok(Number.isFinite(Date.parse(context.captured_at)));
  assert.equal(pageContext({url:'https://example.com',title:'Example',text:'selection',selection:true}).source,'selection');
  assert.throws(()=>pageContext({url:'file:///secret',title:'',text:''}));
});
