import {setLanguage} from '../app/utils/i18n.ts';
setLanguage('ru');
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {parseUiDocument,sourceRows,readUiPath,resolveUiProps} from '../app/utils/ui-document.ts';

const doc=body=>({version:1,title:'Nutrition',body,sources:[{id:'meals',data:[{meal:'Breakfast',kcal:350}]}]});
test('MDC parses native blocks and code without resolving arbitrary Vue components',async()=>{
  const parsed=await parseUiDocument(doc('# Nutrition\n\n::card{title="Today"}\n:::metric{label="Calories" value="350"}\n:::\n::\n\n::data-table{source="meals"}\n::\n\n```js\nalert(1)\n```'));
  assert.equal(parsed.nodes[1].tag,'card');
  assert.equal(parsed.nodes[2].tag,'data-table');
  assert.deepEqual(sourceRows(parsed.document.sources[0].data),[{meal:'Breakfast',kcal:350}]);
});
test('bindings read own fields without walking prototypes or invoking functions',()=>{
  assert.equal(readUiPath('slot.row.original.kcal',{slot:{row:{original:{kcal:350}}}}),350);
  assert.equal(readUiPath('slot.constructor',{slot:{}}),undefined);
  assert.equal(readUiPath('slot.secret',{slot:Object.create({secret:'hidden'})}),undefined);
});
test('inline code discards MDC class metadata',async()=>{
  const parsed=await parseUiDocument(doc('Inline `value` text.'));
  const code=parsed.nodes[0].children.find(node=>node.tag==='code');
  assert.deepEqual(code.props,{});
  assert.equal(code.children[0].value,'value');
});
test('MDC rejects component escapes, bindings, handlers, styles and unsafe URLs',async()=>{
  for(const body of ['::nuxt-link{to="/auth/logout"}\n::','::card{:title="runtime"}\n::',
    '::card{style="position:fixed"}\n::',
    '---\nmdc:\n  components:\n    p: NuxtLink\n---\nhello',
    '::data-table{source="unknown"}\n::','{{ constructor }}']) {
    await assert.rejects(()=>parseUiDocument(doc(body)),undefined,body);
  }
  const stripped=await parseUiDocument(doc('::card{onclick="alert(1)"}\n::\n\n[click](javascript:alert%281%29)'));
  assert.deepEqual(stripped.nodes[0].props,{});
  assert.deepEqual(stripped.nodes[1].children[0].props,{});
});
test('dataset records and summaries become bounded scalar rows',()=>{
  assert.deepEqual(sourceRows({records:[{id:'a',revision:1,values:{kcal:350}}]}),[{id:'a',kcal:350}]);
  assert.deepEqual(sourceRows({groups:[{group:'2026-10-20',count:2,metrics:{kcal:{sum:1000,average:500,count:2}}}]}),[{group:'2026-10-20',count:2,kcal:1000}]);
  assert.deepEqual(sourceRows({records:[{id:'a',values:{constructor:'C',prototype:'P'}}]}),[{id:'a',constructor:'C',prototype:'P'}]);
});

test('Nuxt UI JSON props, named/scoped slots and sources parse as declarative nodes',async()=>{
  const parsed=await parseUiDocument(doc('::u-card\n#header\nNative card\n#default\n:::u-select{:items=\'["A","B"]\' :disabled="false"}\n:::\n::\n\n::u-table{:data="sources.meals"}\n#meal-cell\n{{ slot.row.original.meal }}\n::'));
  assert.equal(parsed.nodes[0].tag,'u-card');
  assert.equal(parsed.nodes[0].children[0].slot,'header');
  assert.deepEqual(parsed.nodes[0].children[1].props.items,['A','B']);
  assert.equal(parsed.nodes[0].children[1].props.disabled,false);
  assert.equal(parsed.nodes[1].bindings.data,'sources.meals');
  assert.equal(parsed.nodes[1].children[0].children[0].children[0].value,'slot.row.original.meal');
});

test('Nuxt UI classes from MDC normalize to the supported Vue class prop',async()=>{
  for(const attributes of ['class="p-4 text-center"','.p-4.text-center']) {
    const parsed=await parseUiDocument(doc(`::u-card{${attributes}}\nCard\n::`));
    assert.deepEqual(parsed.nodes[0].props.class,['p-4','text-center']);
    assert.equal(Object.hasOwn(parsed.nodes[0].props,'className'),false);
  }
  for(const body of ['::card{class="p-4"}\nCard\n::','[Link](https://example.com){.p-4}']) {
    await assert.rejects(()=>parseUiDocument(doc(body)),/не поддерживается/);
  }
});

test('Nuxt UI rejects excluded components, executable props and unsafe nested URLs',async()=>{
  for(const body of ['::u-editor\n::','::u-chat-prompt\n::','::u-app\n::','::u-page-grid\n::',
    '::u-select{:items="fetch(\'/secret\')"}\n::','::u-card{as="script"}\n::',
    '::u-card{:as=\'{"template":"<script>bad()</script>"}\'}\n::',
    '::u-button{:to=\'{"path":"javascript:alert(1)"}\'}\n::',
    '::u-dropdown-menu{:items=\'[{"label":"X","onSelect":"run()"}]\'}\n::',
    '::u-card{:ui=\'{"__proto__":{"x":1}}\'}\n::',
    '::u-table{:data="sources.unknown"}\n::','::u-badge{:label="slot.constructor"}\n::']) await assert.rejects(()=>parseUiDocument(doc(body)),undefined,body);
});

test('default scoped slots can read their own item values',async()=>{
  const parsed=await parseUiDocument(doc('::u-carousel{:items=\'["One","Two"]\'}\n{{ slot.item }}\n::'));
  assert.equal(parsed.nodes[0].children[0].children[0].value,'slot.item');
});

test('computed views bind chart rows and scalar progress fields',async()=>{
  const raw={...doc('::u-chart{type="line" :data="views.week" index="date" :categories=\'["protein","fat","carbs"]\'}\n::\n\n::u-progress{:model-value="views.latest.kcal" :max="views.latest.target"}\n::'),
    views:{week:[{date:'2026-10-21',protein:100,fat:60,carbs:200}],latest:{kcal:1900,target:2200}}};
  const parsed=await parseUiDocument(raw);
  assert.equal(parsed.nodes[0].bindings.data,'views.week');
  assert.equal(parsed.nodes[1].bindings.modelValue,'views.latest.kcal');
  for(const views of [{...raw.views,week:{}},{...raw.views,week:[{date:'today',protein:'bad',fat:60,carbs:200}]},{...raw.views,latest:{kcal:'bad',target:2200}}]) {
    await assert.rejects(()=>parseUiDocument({...raw,views}));
  }
  await assert.rejects(()=>parseUiDocument({...raw,views:{...raw.views,latest:{kcal:1900}}}));
  await parseUiDocument({...raw,views:{week:[],latest:null}});
});

test('segmented progress accepts literal and computed portions of one scale',async()=>{
  const segments=[{value:80,color:'primary',label:'Основное'},{value:20,color:'error',label:'Превышение'}];
  const raw={...doc('::u-progress{:segments="views.portions" :max="100"}\n::'),views:{portions:segments}};
  const parsed=await parseUiDocument(raw);
  assert.deepEqual(resolveUiProps(parsed.nodes[0],{views:raw.views}).segments,segments);
  const literal=await parseUiDocument(doc(`::u-progress{:segments='${JSON.stringify(segments)}' :max="100"}\n::`));
  assert.deepEqual(literal.nodes[0].props.segments,segments);
  for(const value of [[],[{value:0,color:'primary'}],[{value:80,color:'success'}]]) await parseUiDocument({...raw,views:{portions:value}});
  await parseUiDocument({...raw,views:{portions:null}});
  await parseUiDocument(doc('::u-progress{:segments=\'[{"value":0.1,"color":"primary"},{"value":0.2,"color":"error"}]\' :max="0.3"}\n::'));
});

test('segmented progress rejects invalid or ambiguous scales',async()=>{
  for(const segments of [{},[{value:-1,color:'primary'}],[{value:'80',color:'primary'}],[{value:80,color:'url(/secret)'}],[{value:101,color:'primary'}],[{value:80,color:'primary',unknown:true}],Array.from({length:33},()=>({value:0,color:'primary'}))]) {
    await assert.rejects(()=>parseUiDocument({...doc('::u-progress{:segments="views.portions" :max="100"}\n::'),views:{portions:segments}}));
  }
  await assert.rejects(()=>parseUiDocument(doc('::u-progress{:segments=\'[{"value":80,"color":"primary"}]\' :model-value="80"}\n::')));
});

test('table headers and dates parse without rewriting source values',async()=>{
  const columns=[{accessorKey:'date',header:'День',dateFormat:{day:'2-digit',month:'long'}},{accessorKey:'kcal',header:'Калории, ккал'}];
  const raw={...doc(`::data-table{source="days" :columns='${JSON.stringify(columns)}'}\n::\n\n::u-table{:data="sources.days" :columns='${JSON.stringify(columns)}'}\n#date-cell\n:::date-value{:value="slot.row.original.date" date-format="long"}\n:::\n::\n\n:date-value{:value="views.day" :date-format='{"dateStyle":"long"}'}`),sources:[{id:'days',data:[{date:'2026-10-21',kcal:100}]}],views:{day:'2026-10-21'}};
  const parsed=await parseUiDocument(raw);
  assert.deepEqual(parsed.nodes[0].props.columns,columns);
  assert.equal(parsed.nodes[1].props.columns[0].header,'День');
  assert.equal(parsed.document.sources[0].data[0].date,'2026-10-21');
  await parseUiDocument({...doc('::date-value{:value="views.day"}\n::'),views:{day:null}});
  for(const column of [{accessorKey:'date',dateFormat:{timeZone:'invalid'}},{accessorKey:'date',dateFormat:{unknown:1}}]) {
    for(const body of [`::data-table{source="meals" :columns='${JSON.stringify([column])}'}\n::`,`::u-table{:data="sources.meals" :columns='${JSON.stringify([column])}'}\n::`]) await assert.rejects(()=>parseUiDocument(doc(body)));
  }
  for(const columns of [[{accessorKey:'date'},{accessorKey:'date'}],[{accessorKey:'constructor'}],[{accessorKey:'date',unknown:1}],{}]) {
    await assert.rejects(()=>parseUiDocument(doc(`::data-table{source="meals" :columns='${JSON.stringify(columns)}'}\n::`)));
  }
  for(const body of ['::date-value{value="2026-02-30"}\n::','::date-value{value="2026-10-21" :date-format=\'{"timeZone":"invalid"}\'}\n::','::date-value\n::']) await assert.rejects(()=>parseUiDocument(doc(body)));
});

test('columns accepts one to four desktop columns and preserves the default',async()=>{
  for(const attributes of ['count="4"',':count="4"']) {
    const parsed=await parseUiDocument(doc(`::columns{${attributes}}\n:::card{title="A"}\n:::\n::`));
    assert.equal(Number(parsed.nodes[0].props.count),4);
  }
  for(const count of [1,2,3]) await parseUiDocument(doc(`::columns{count="${count}"}\n::`));
  assert.deepEqual((await parseUiDocument(doc('::columns\n::'))).nodes[0].props,{});
  for(const count of ['0','5','2.5','auto','NaN']) await assert.rejects(()=>parseUiDocument(doc(`::columns{count="${count}"}\n::`)));
});

test('UChart range adapter accepts a band without boundary line categories',async()=>{
  const raw={...doc('::u-chart{:data="views.week" index="date" :range=\'{"min":"low","max":"high"}\'}\n::'),views:{week:[{date:'2026-10-21',low:100,high:200}]}};
  assert.deepEqual((await parseUiDocument(raw)).nodes[0].props.range,{min:'low',max:'high'});
  await assert.rejects(()=>parseUiDocument({...raw,views:{week:[{date:'2026-10-21',low:200,high:100}]}}));
  await assert.rejects(()=>parseUiDocument({...raw,views:{week:{records:[{id:'a',values:{date:'2026-10-21',low:100,high:200}}]}}}));
  await parseUiDocument({...raw,views:{week:[]}});
});
