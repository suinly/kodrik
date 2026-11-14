import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readDocument} from '../documents.mjs';

async function fixture(options, run) {
  const old={document:globalThis.document,location:globalThis.location,fetch:globalThis.fetch};
  globalThis.document={contentType:options.type ?? 'application/pdf',title:'Report'};
  globalThis.location={href:options.url ?? 'https://example.com/report'};
  const calls=[];
  globalThis.fetch=async (url,init)=>{calls.push({url,init});return options.response ?? new Response('%PDF-1.7\ncontent',{headers:{'content-type':'application/pdf'}})};
  try {await run(calls)} finally {Object.assign(globalThis,old)}
}
test('PDF capture reads original bytes with credentials, metadata-only preview does not fetch',async()=>{
  await fixture({},async calls=>{
    const preview=await readDocument('https://example.com/report',true);
    assert.equal(calls.length,0);assert.equal(preview.name,'Report.pdf');
    const result=await readDocument('https://example.com/report',false);
    assert.equal(atob(result.base64),'%PDF-1.7\ncontent');
    assert.equal(calls[0].init.credentials,'include');assert.equal(calls[0].init.redirect,'error');
  });
});
test('capture fails closed on navigation, wrong MIME, invalid PDF, and excessive size',async()=>{
  for(const options of [
    {url:'https://example.com/changed'},
    {response:new Response('<html>login</html>',{headers:{'content-type':'text/html'}})},
    {response:new Response('not pdf',{headers:{'content-type':'application/pdf'}})},
    {response:new Response('%PDF-',{headers:{'content-type':'application/pdf','content-length':'20000001'}})},
  ]) await fixture(options,async()=>assert.rejects(()=>readDocument('https://example.com/report',false)));
});
test('Google Workspace exports native documents instead of scraping the editor',async()=>{
  for(const [kind,format,mime,bytes] of [
    ['document','md','text/markdown','Hello document'],
    ['presentation','pdf','application/pdf','%PDF-1.7'],
    ['spreadsheets','xlsx','application/vnd.openxmlformats-officedocument.spreadsheetml.sheet','PK\x03\x04data'],
  ]) await fixture({type:'text/html',url:`https://docs.google.com/${kind}/d/abc/edit?tab=t.1`,response:new Response(bytes,{headers:{'content-type':mime}})},async calls=>{
    const result=await readDocument(location.href,false);
    assert.equal(result.mime,mime);assert.equal(atob(result.base64),bytes);
    const target=new URL(calls[0].url);assert.equal(target.searchParams.get('format'),format);assert.equal(target.searchParams.get('tab'),'t.1');
  });
});
test('ordinary sites do not trigger document requests',async()=>{
  await fixture({type:'text/html'},async calls=>{assert.equal(await readDocument(location.href,false),null);assert.equal(calls.length,0)});
});
