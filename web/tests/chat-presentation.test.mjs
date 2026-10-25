import {setLanguage} from '../app/utils/i18n.ts';
setLanguage('ru');
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {groupMessageParts,supersededDocuments,shortConversationTitle,matchingMessages,toolSummary,toolLabel,formatMessageTime} from '../app/utils/chat-presentation.ts';

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
    ['unknown',{payload:'large payload'},''],
    ['constructor',{cmd:'hidden'},''],
    ['exec_command',null,''],
    ['exec_command',{cmd:{nested:'value'}},''],
  ]) assert.equal(toolSummary({...tool('a',name),input}),expected);
  assert.equal(toolSummary({...tool('a'),input:{cmd:'я'.repeat(200)}}),'я'.repeat(95)+'…');
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
