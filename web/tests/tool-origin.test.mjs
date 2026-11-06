import {test} from 'node:test';
import assert from 'node:assert/strict';
import {expect} from '@playwright/test';
import {componentPage} from './devices.test.mjs';

test('tool history renders literal origin and a stale-generation local-file notice without fetching paths',async()=>{
  const origin={label:'<img src=x onerror=alert(1)>',project_root:'/work/<script>unsafe()</script>',tool_id:'read_file',environment:{connection_id:'connection',generation:'old'}};
  const tool={type:'dynamic-tool',toolCallId:'call',toolName:'local__read_file',state:'output-available',input:{path:'/work/private.txt'},output:{text:'read before disconnect'},origin};
  const {page,browser,mount}=await componentPage('ChatToolSteps',{tools:[tool],environment:{session_id:'s',label:'Laptop',project_root:'/work',state:'connected',environment:{connection_id:'connection',generation:'new'}}});
  try {
    await page.evaluate(()=>{
      const {h}=VueForTests;
      app.component('UChatTool',{props:['text'],setup:(props,{slots})=>()=>h('section',[h('h3',props.text),slots.default?.()])});
    });
    const requests=[];page.on('request',request=>requests.push(request.url()));
    await mount();
    await expect(page.getByRole('heading',{name:`read_file · ${origin.label}`,exact:true})).toBeVisible();
    await page.evaluate(()=>{
      props.tools[0].toolName='local__exec__719e6926f958f45893bc00ee50e2e1ae';
      props.tools[0].origin.tool_id='exec_command';
      props.tools[0].origin.label='macbook-pro';
    });
    await expect(page.getByRole('heading',{name:'exec_command · macbook-pro',exact:true})).toBeVisible();
    await page.evaluate(origin=>{
      props.tools[0].toolName='local__read_file';props.tools[0].origin=origin;
    },origin);
    await expect(page.getByText(origin.label+' · '+origin.project_root,{exact:true})).toBeVisible();
    await expect(page.getByRole('status')).toHaveText('The file remains on the disconnected device.');
    assert.equal(await page.locator('#app img,#app script').count(),0);
    await page.evaluate(()=>{props.environment.environment.generation='old'});
    await expect(page.getByRole('status')).toHaveText('The local file is not saved on the server.');
    assert.deepEqual(requests,[]);
  } finally {await browser.close()}
});
