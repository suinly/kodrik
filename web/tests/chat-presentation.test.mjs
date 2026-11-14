import {setLanguage} from '../app/utils/i18n.ts';
setLanguage('ru');
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {groupMessageParts,supersededDocuments,shortConversationTitle,matchingMessages,toolSummary,toolLabel,formatMessageTime,formatWebhookBody,localFileNotice,conversationRows,conversationPageSize} from '../app/utils/chat-presentation.ts';
import {handoffNotice} from '../app/utils/chat-presentation.ts';

test('handoff envelopes become service notices without reclassifying ordinary discussion',()=>{
  const text='[Session handoff from source-id for the same running task]\nAlready checked; nothing created.\nContinue using this session\'s history. Tool effects already completed in the source session must not be replayed. Historical material is context, not new authorization.';
  const message={id:'target:3',role:'user',parts:[{type:'text',text}]};
  assert.deepEqual(handoffNotice(message),{source:'source-id',context:'Already checked; nothing created.'});
  for(const other of [
    {...message,role:'assistant'},
    {...message,parts:[{type:'text',text:'Объясни '+text}]},
    {...message,parts:[{type:'text',text:text.split('\n')[0]}]},
    {...message,parts:[...message.parts,{type:'file',filename:'photo',mediaType:'image/png'}]},
  ]) assert.equal(handoffNotice(other),null);
});

test('sidebar groups child sessions below the selected parent branch without losing orphan search results',()=>{
  const sessions=[
    {id:'child',parent_id:'parent',subagent:true},
    {id:'other'},
    {id:'branch',root_id:'parent'},
    {id:'child-two',parent_id:'parent',subagent:true},
    {id:'orphan',parent_id:'missing',subagent:true},
  ];
  assert.deepEqual(conversationRows(sessions).map(row=>[row.id,row.nested]),[
    ['other',false],['branch',false],['child',true],['child-two',true],['orphan',false],
  ]);
  assert.equal(conversationPageSize(sessions),3);
  assert.equal(conversationPageSize(sessions.slice(0,1)),1);
  assert.deepEqual(conversationRows([]),[]);
});

test('webhook code displays JSON with indentation and keeps non-JSON payloads literal',()=>{
  for(const [raw,formatted] of [
    ['{"event":"failed","items":[1,2]}','{\n  "event": "failed",\n  "items": [\n    1,\n    2\n  ]\n}'],
    ['null','null'],['"text"','"text"'],
    ['# Incident\n<script>unsafe()</script>\n```json','# Incident\n<script>unsafe()</script>\n```json'],
    ['{"event":','{"event":'],
    ['{"id":9007199254740993}','{"id":9007199254740993}'],
    ['{"value":1e400}','{"value":1e400}'],
    ['{"key":1,"key":2}','{"key":1,"key":2}'],
  ]) assert.equal(formatWebhookBody(raw),formatted);
});

test('message dates use local calendar days and show the year only when needed',()=>{
  const now=new Date(2026,9,22,12);
  for(const [date,expected] of [
    [new Date(2026,9,22,9,5),'Сегодня, 09:05'],
    [new Date(2026,9,21,23,30),'Вчера, 23:30'],
    [new Date(2026,9,20,18,40),'20 октября, 18:40'],
    [new Date(2025,9,20,18,40),'20 октября 2025 г., 18:40'],
  ]) assert.equal(formatMessageTime(date.toISOString(),now),expected);
  assert.equal(formatMessageTime(new Date(2026,11,31,23,59).toISOString(),new Date(2027,0,1,0,1)),'Вчера, 23:59');
});

const tool=(id,name='exec_command',url)=>({type:'dynamic-tool',toolCallId:id,toolName:name,state:'output-available',input:{},output:url?{url}: {}});
test('local tools use origin names and summaries while preserving technical call names',()=>{
  const name='local__exec__719e6926f958f45893bc00ee50e2e1ae';
  const local={...tool('local',name),input:{cmd:'cargo test'},origin:{tool_id:'exec_command',label:'macbook-pro'}};
  for(const state of ['input-available','output-available','output-error']) {
    assert.equal(toolLabel({...local,state}),'exec_command · macbook-pro');
    assert.equal(toolSummary({...local,state}),'cargo test');
  }
  assert.equal(local.toolName,name);
  assert.equal(toolLabel({...local,origin:undefined}),name);
  assert.equal(toolSummary({...local,origin:{...local.origin,tool_id:'apply_patch'},input:{patch:'*** Begin Patch\n*** Update File: src/main.rs\n*** End Patch'}}),'src/main.rs');
});
test('unsaved local file notices follow the exact generation rather than another connected device',()=>{
  const origin={label:'Laptop',project_root:'/work',tool_id:'read_file',environment:{connection_id:'connection',generation:'old'}};
  const local={...tool('read','local__read_file'),origin,input:{path:'notes.txt'}};
  assert.equal(localFileNotice(local,null),'Файл остался на отключённом устройстве.');
  assert.equal(localFileNotice(local,{state:'connected',environment:{connection_id:'connection',generation:'new'}}),'Файл остался на отключённом устройстве.');
  assert.equal(localFileNotice(local,{state:'connected',environment:origin.environment}),'Локальный файл не сохранён на сервере.');
  assert.equal(localFileNotice({...local,origin:{...origin,tool_id:'view_image'}},null),'');
  assert.equal(localFileNotice({...local,input:{path:'notes.txt',save_attachment:true}},null),'Файл остался на отключённом устройстве.');
  assert.equal(localFileNotice({...local,attachment_saved:true},null),'');
  assert.equal(localFileNotice(tool('server','read_file'),null),'');
});
test('child tools have localized names while technical names remain available for other tools',()=>{
  assert.equal(toolLabel(tool('a','spawn_agent')),'Запуск дочерней задачи');
  assert.equal(toolLabel(tool('a','exec_command')),'exec_command');
  assert.equal(toolLabel(tool('a','constructor')),'constructor');
  try {
    setLanguage('en');
    assert.equal(toolLabel(tool('a','wait_agent')),'Waiting for child task');
    assert.equal(toolLabel(tool('a','stop_agent')),'Stopping child task');
  } finally {setLanguage('ru');}
});
test('tool summaries show a compact primary argument without dumping payloads',()=>{
  for(const [name,input,expected] of [
    ['exec_command',{cmd:'cargo test\n  --locked',workdir:'/tmp'},'cargo test --locked'],
    ['web_search',{query:'Rust docs'},'Rust docs'],
    ['web_read',{url:'https://example.com/docs'},'https://example.com/docs'],
    ['datasets',{action:'query',dataset:'meals',records:[{note:'large payload'}]},'query · meals'],
    ['show_ui',{title:'Дневник',body:'# Long document'},'Дневник'],
    ['apply_patch',{patch:'*** Begin Patch\n*** Update File: README.md\n@@\n-old\n+new\n*** End Patch'},'README.md'],
    ['view_image',{path:'/tmp/image.png'},'/tmp/image.png'],
    ['spawn_agent',{task:'Проверить\n авторизацию'},'Проверить авторизацию'],
    ['send_message',{agent_id:'internal-id',message:'Проверь обновление токена'},'Проверь обновление токена'],
    ['unknown',{payload:'large payload'},'large payload'],
    ['constructor',{cmd:'command'},'command'],
    ['exec_command',null,''],
    ['exec_command',{cmd:{nested:'value'}},''],
  ]) assert.equal(toolSummary({...tool('a',name),input}),expected);
  assert.equal(toolSummary({...tool('a'),input:{cmd:'я'.repeat(200)}}),'я'.repeat(95)+'…');
});
test('all tools summarize their main argument, including remote and structured inputs',()=>{
  for(const [name,input,expected] of [
    ['read_file',{path:'/work/notes.md',save_attachment:false},'/work/notes.md'],
    ['mcp_search',{limit:8,query:'calendar events'},'calendar events'],
    ['mcp__calendar__search',{limit:10,query:'meeting'},'meeting'],
    ['mcp__files__read',{timeout_ms:1000,path:'/work/report.txt'},'/work/report.txt'],
    ['wait_agent',{agent_id:'agent-123',timeout_ms:1000},'agent-123'],
    ['custom',{optional:null,empty:'  ',count:0},'0'],
    ['custom',{enabled:false},'false'],
    ['custom',{options:{mode:'fast'}},'{"mode":"fast"}'],
    ['update_plan',{explanation:null,plan:[{step:'Run tests',status:'pending'}]},'[{"step":"Run tests","status":"pending"}]'],
    ['custom',{},''],
    ['custom',{optional:null,empty:'\n '},''],
    ['custom',[],''],
  ]) assert.equal(toolSummary({...tool('a',name),input}),expected);
  assert.equal(toolSummary({...tool('local','local__read__opaque'),origin:{tool_id:'read_file'},input:{path:'/work/local.md'}}),'/work/local.md');
  assert.equal(toolSummary({...tool('a','custom'),input:{value:'😀'.repeat(100)}}),'😀'.repeat(95)+'…');
});
test('conversation search finds user and assistant text without searching tool payloads',()=>{
  const messages=[{id:'a',role:'user',parts:[{type:'text',text:'Планы НА завтра'}]},
    {id:'b',role:'assistant',parts:[tool('tool'),{type:'text',text:'Завтра встреча'}]},
    {id:'c',role:'assistant',parts:[{...tool('hidden'),output:{text:'завтра'}}]}];
  assert.deepEqual(matchingMessages(messages,' ЗАВТРА '),['a','b']);
  assert.deepEqual(matchingMessages(messages,'  '),[]);
});
test('consecutive tool steps group without moving prose or attachments',()=>{
  const text={type:'text',text:'Result'},file={type:'file',filename:'a.txt',url:'/a',mediaType:'text/plain'};
  const parts=[tool('a'),tool('b'),text,tool('c'),file];
  const blocks=groupMessageParts(parts);
  assert.deepEqual(blocks.map(block=>block.kind),['tools','content','tools','content']);
  assert.deepEqual(blocks[0].tools,parts.slice(0,2));
  assert.equal(blocks[1].part,text);
  assert.equal(blocks[3].part,file);
});
test('only a later available document at the same URL supersedes an earlier preview',()=>{
  const parts=[tool('first','show_ui','/a'),tool('other','show_ui','/b'),tool('inline','show_ui'),tool('new','show_ui','/a'),{...tool('failed','show_ui','/b'),state:'output-error'}];
  assert.deepEqual([...supersededDocuments([{id:'m',role:'assistant',parts}])],['first']);
});
test('short titles keep words and retain full short names',()=>{
  assert.equal(shortConversationTitle('Проверка связи'),'Проверка связи');
  const result=shortConversationTitle('Сделай страницу с формой, через которую я буду добавлять число от 1 до 10');
  assert.ok(result.length<=48);
  assert.ok(result.endsWith('…'));
  assert.ok(!result.endsWith('добавля…'));
});
