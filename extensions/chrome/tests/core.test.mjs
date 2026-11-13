import {test} from 'node:test';
import assert from 'node:assert/strict';
import {serverURL, truncateUTF8, contextText} from '../core.mjs';

test('server address keeps deployment prefix and rejects credentials, queries and non-web schemes',()=>{
  assert.equal(serverURL('https://example.com/kodrik').href,'https://example.com/kodrik/');
  for(const value of ['javascript:alert(1)','https://user:secret@example.com','https://example.com/?token=x','https://example.com/#x','http://example.com']) assert.throws(()=>serverURL(value));
  assert.equal(serverURL('http://127.0.0.1:8080/').origin,'http://127.0.0.1:8080');
});
test('UTF-8 truncation never splits unicode or exceeds budget',()=>{
  assert.equal(truncateUTF8('абв🙂',7),'абв');
  assert.equal(truncateUTF8('abc',3),'abc');
  assert.equal(truncateUTF8('🙂',3),'');
});
test('context is explicitly untrusted and byte-bounded including metadata',()=>{
  const text=contextText({url:'https://example.com',title:'Example',text:'я'.repeat(90000),selection:false});
  assert.ok(new TextEncoder().encode(text).length<=100000);
  assert.match(text,/UNTRUSTED/);assert.match(text,/https:\/\/example.com/);assert.match(text,/TRUNCATED/);
  assert.throws(()=>contextText({url:'file:///secret',title:'',text:''}));
});
